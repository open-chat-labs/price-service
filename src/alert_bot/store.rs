//! The alerts waiting to fire, in memory and in a JSON file, so that they outlast restarts and
//! deploys. Each change is written to the file as it's made.

use crate::prices::{Source, now};
use anyhow::{Context, Result};
use oc_bots_sdk::types::{
    CanisterId, Chat, EventIndex, InstallationLocation, MessageIndex, TimestampMillis, UserId,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{error, warn};

/// The most alerts someone can have waiting in a chat
pub const MAX_PER_USER_PER_CHAT: usize = 20;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Above,
    Below,
}

impl Direction {
    /// Whether `price` has reached `target`, from this side
    pub fn is_met(self, price: f64, target: f64) -> bool {
        match self {
            Direction::Above => price >= target,
            Direction::Below => price <= target,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Direction::Above => "above",
            Direction::Below => "below",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Alert {
    pub id: u64,
    /// Who set it, and is told when it fires
    pub user: UserId,
    /// Where it was set, and is posted
    pub chat: Chat,
    /// The thread it was set in, if it was
    #[serde(default)]
    pub thread: Option<MessageIndex>,
    /// The message the bot confirmed it with, in the chat or the thread, which it replies to when
    /// it fires
    #[serde(default)]
    pub confirmation: Option<EventIndex>,
    /// The canister the bot posts in the chat through
    pub api_gateway: CanisterId,
    pub symbol: String,
    pub source: Source,
    pub direction: Direction,
    /// In USD
    pub target: f64,
    pub set_at: TimestampMillis,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Saved {
    next_id: u64,
    // Oldest first
    alerts: Vec<Alert>,
    // Those whose confirmation is being posted, which aren't checked until it is, so that they
    // reply to it. They're only kept in memory: after a restart, they fire without a reply.
    #[serde(skip)]
    confirming: HashSet<u64>,
}

pub struct Alerts {
    file: Option<PathBuf>,
    saved: Mutex<Saved>,
}

impl Alerts {
    /// The alerts in `file`, which each change is written to. Without a file they're only kept in
    /// memory, eg. in tests. A file that can't be parsed, eg. one cut short, is moved aside, to
    /// start with no alerts rather than not at all.
    pub fn load(file: Option<PathBuf>) -> Result<Self> {
        let saved = match &file {
            Some(file) if file.exists() => {
                let json = std::fs::read(file)
                    .with_context(|| format!("Failed to read the alerts in {}", file.display()))?;
                match serde_json::from_slice(&json) {
                    Ok(saved) => saved,
                    Err(e) => {
                        let aside = file.with_extension(format!("json.unreadable-{}", now()));
                        std::fs::rename(file, &aside).with_context(|| {
                            format!("Failed to move the alerts in {} aside", file.display())
                        })?;
                        warn!(error = %e, aside = %aside.display(), "The alerts couldn't be parsed, so they were moved aside");
                        Saved::default()
                    }
                }
            }
            _ => Saved::default(),
        };
        Ok(Alerts {
            file,
            saved: Mutex::new(saved),
        })
    }

    /// Adds the alert, giving it an id, unless its user has as many as they can have in its chat.
    /// It's only added once it's saved, so an alert someone's told failed to set never fires.
    pub fn add(&self, mut alert: Alert) -> Result<Option<Alert>> {
        let mut saved = self.saved.lock().unwrap();
        let in_chat = saved
            .alerts
            .iter()
            .filter(|a| a.user == alert.user && a.chat == alert.chat)
            .count();
        if in_chat >= MAX_PER_USER_PER_CHAT {
            return Ok(None);
        }

        let mut changed = saved.clone();
        changed.next_id += 1;
        alert.id = changed.next_id;
        changed.alerts.push(alert.clone());
        changed.confirming.insert(alert.id);
        if let Some(file) = &self.file {
            write(file, &changed)?;
        }
        *saved = changed;
        Ok(Some(alert))
    }

    /// The user's alerts in the chat, oldest first, as `/alerts` numbers them
    pub fn of(&self, user: UserId, chat: Chat) -> Vec<Alert> {
        self.saved
            .lock()
            .unwrap()
            .alerts
            .iter()
            .filter(|a| a.user == user && a.chat == chat)
            .cloned()
            .collect()
    }

    /// Notes the message the alert was confirmed with, after which it's checked
    pub fn confirmed(&self, id: u64, confirmation: EventIndex) {
        self.change(|saved| {
            saved.confirming.remove(&id);
            if let Some(alert) = saved.alerts.iter_mut().find(|a| a.id == id) {
                alert.confirmation = Some(confirmation);
            }
        })
    }

    /// Removes the alert confirmed with the message at `confirmation`, eg. as it was deleted
    pub fn remove_confirmed(
        &self,
        chat: Chat,
        thread: Option<MessageIndex>,
        confirmation: EventIndex,
    ) -> Option<Alert> {
        self.change(|saved| {
            let index = saved.alerts.iter().position(|a| {
                a.chat == chat && a.thread == thread && a.confirmation == Some(confirmation)
            })?;
            Some(saved.alerts.remove(index))
        })
    }

    /// Removes the user's `n`th alert in the chat, counting from 1, as `/alerts` numbers them
    pub fn remove_nth(&self, user: UserId, chat: Chat, n: usize) -> Option<Alert> {
        self.change(|saved| {
            let index = saved
                .alerts
                .iter()
                .enumerate()
                .filter(|(_, a)| a.user == user && a.chat == chat)
                .nth(n.checked_sub(1)?)
                .map(|(i, _)| i)?;
            Some(saved.alerts.remove(index))
        })
    }

    /// Removes the alert, eg. once it's fired. Returns whether it was there.
    pub fn remove(&self, id: u64) -> bool {
        self.change(|saved| {
            saved.confirming.remove(&id);
            let before = saved.alerts.len();
            saved.alerts.retain(|a| a.id != id);
            saved.alerts.len() < before
        })
    }

    /// Removes the alerts set by `at` in the chats the bot was uninstalled from: a group, a direct
    /// chat, or all a community's channels. Those set since, eg. after it was installed again, are
    /// kept, as the notice can come late. Returns how many were removed.
    pub fn remove_at(&self, location: InstallationLocation, at: TimestampMillis) -> usize {
        self.change(|saved| {
            let before = saved.alerts.len();
            saved
                .alerts
                .retain(|a| InstallationLocation::from(a.chat) != location || a.set_at > at);
            before - saved.alerts.len()
        })
    }

    #[cfg(test)]
    pub fn all(&self) -> Vec<Alert> {
        self.saved.lock().unwrap().alerts.clone()
    }

    /// The alerts to check: all but those whose confirmation is still being posted
    pub fn checkable(&self) -> Vec<Alert> {
        let saved = self.saved.lock().unwrap();
        saved
            .alerts
            .iter()
            .filter(|a| !saved.confirming.contains(&a.id))
            .cloned()
            .collect()
    }

    // Every change but an addition is kept even if it can't be saved, as a fired alert left in
    // memory would be posted again at every refresh. The file catches up at the next change that's
    // saved.
    fn change<T>(&self, f: impl FnOnce(&mut Saved) -> T) -> T {
        let mut saved = self.saved.lock().unwrap();
        let result = f(&mut saved);
        if let Some(file) = &self.file
            && let Err(e) = write(file, &saved)
        {
            error!(error = ?e, "Failed to save the alerts");
        }
        result
    }
}

// Written to a temporary file, flushed to disk, then renamed over the old one, so a crash or a
// power cut mid-write leaves the old alerts rather than half the new ones
fn write(file: &Path, saved: &Saved) -> Result<()> {
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    let temporary = file.with_extension("json.tmp");
    let mut written = std::fs::File::create(&temporary)
        .with_context(|| format!("Failed to create {}", temporary.display()))?;
    written
        .write_all(&serde_json::to_vec(saved)?)
        .and_then(|()| written.sync_all())
        .with_context(|| format!("Failed to write {}", temporary.display()))?;
    std::fs::rename(&temporary, file)
        .with_context(|| format!("Failed to replace {}", file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_agent::export::Principal;

    fn id(n: u8) -> CanisterId {
        Principal::from_slice(&[n; 10]).into()
    }

    fn alert(user: u8, chat: Chat) -> Alert {
        Alert {
            id: 0,
            user: id(user),
            chat,
            thread: None,
            confirmation: None,
            api_gateway: id(100),
            symbol: "CHAT".to_string(),
            source: Source::Icp(id(200)),
            direction: Direction::Above,
            target: 0.05,
            set_at: 1,
        }
    }

    const GROUP: u8 = 10;

    fn group() -> Chat {
        Chat::Group(id(GROUP))
    }

    #[test]
    fn above_is_met_at_or_over_the_target_and_below_at_or_under_it() {
        assert!(Direction::Above.is_met(0.05, 0.05));
        assert!(Direction::Above.is_met(0.06, 0.05));
        assert!(!Direction::Above.is_met(0.04, 0.05));
        assert!(Direction::Below.is_met(0.05, 0.05));
        assert!(Direction::Below.is_met(0.04, 0.05));
        assert!(!Direction::Below.is_met(0.06, 0.05));
    }

    #[test]
    fn alerts_are_numbered_per_user_per_chat_oldest_first() {
        let alerts = Alerts::load(None).unwrap();
        let first = alerts.add(alert(1, group())).unwrap().unwrap();
        alerts.add(alert(2, group())).unwrap();
        let second = alerts.add(alert(1, group())).unwrap().unwrap();
        alerts.add(alert(1, Chat::Group(id(11)))).unwrap();

        assert_ne!(first.id, second.id);
        let mine: Vec<u64> = alerts.of(id(1), group()).iter().map(|a| a.id).collect();
        assert_eq!(mine, [first.id, second.id]);

        assert_eq!(alerts.remove_nth(id(1), group(), 2), Some(second));
        assert_eq!(alerts.remove_nth(id(1), group(), 2), None);
        assert_eq!(alerts.remove_nth(id(1), group(), 0), None);
        assert_eq!(alerts.all().len(), 3);
    }

    #[test]
    fn a_user_can_have_20_alerts_in_a_chat() {
        let alerts = Alerts::load(None).unwrap();
        for _ in 0..MAX_PER_USER_PER_CHAT {
            assert!(alerts.add(alert(1, group())).unwrap().is_some());
        }

        assert!(alerts.add(alert(1, group())).unwrap().is_none());
        assert!(alerts.add(alert(2, group())).unwrap().is_some());
        assert!(alerts.add(alert(1, Chat::Group(id(11)))).unwrap().is_some());
    }

    #[test]
    fn uninstalling_drops_the_alerts_in_its_chats() {
        let alerts = Alerts::load(None).unwrap();
        let community = id(20);
        alerts.add(alert(1, group())).unwrap();
        alerts.add(alert(1, Chat::Channel(community, 1))).unwrap();
        alerts.add(alert(2, Chat::Channel(community, 2))).unwrap();
        alerts.add(alert(1, Chat::Group(id(11)))).unwrap();

        let dropped = alerts.remove_at(InstallationLocation::Community(community), 1);
        assert_eq!(dropped, 2);
        assert_eq!(
            alerts.remove_at(InstallationLocation::Group(id(GROUP)), 1),
            1
        );
        assert_eq!(alerts.all()[0].chat, Chat::Group(id(11)));
    }

    #[test]
    fn an_alert_is_only_checked_once_its_confirmed() {
        let alerts = Alerts::load(None).unwrap();
        let confirming = alerts.add(alert(1, group())).unwrap().unwrap();
        let abandoned = alerts.add(alert(1, group())).unwrap().unwrap();
        assert!(alerts.checkable().is_empty());

        alerts.confirmed(confirming.id, 7);
        alerts.remove(abandoned.id);

        let checkable = alerts.checkable();
        assert_eq!(checkable.len(), 1);
        assert_eq!(checkable[0].confirmation, Some(7));
        // and after a restart, an alert whose confirmation was being posted is checked anyway
        let saved = alerts.saved.lock().unwrap().clone();
        let reloaded: Saved = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert!(reloaded.confirming.is_empty());
    }

    #[test]
    fn deleting_a_confirmation_removes_its_alert() {
        let alerts = Alerts::load(None).unwrap();
        let in_chat = alerts.add(alert(1, group())).unwrap().unwrap();
        let in_thread = alerts
            .add(Alert {
                thread: Some(3),
                ..alert(1, group())
            })
            .unwrap()
            .unwrap();
        alerts.confirmed(in_chat.id, 7);
        alerts.confirmed(in_thread.id, 7);

        // Event indexes count within a chat or a thread, so both have to match
        let removed = alerts.remove_confirmed(group(), Some(3), 7).unwrap();
        assert_eq!(removed.id, in_thread.id);
        assert!(alerts.remove_confirmed(group(), None, 8).is_none());
        assert!(
            alerts
                .remove_confirmed(Chat::Group(id(11)), None, 7)
                .is_none()
        );
        assert_eq!(alerts.all().len(), 1);
    }

    #[test]
    fn a_late_uninstall_keeps_the_alerts_set_since() {
        let alerts = Alerts::load(None).unwrap();
        alerts.add(alert(1, group())).unwrap();
        let since = alerts
            .add(Alert {
                set_at: 3,
                ..alert(1, group())
            })
            .unwrap()
            .unwrap();

        assert_eq!(
            alerts.remove_at(InstallationLocation::Group(id(GROUP)), 2),
            1
        );
        assert_eq!(alerts.all(), [since]);
    }

    // A directory where the file should be, so it can't be written
    fn unwritable(test: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("price-alerts-{test}-{}", std::process::id()));
        let file = dir.join("alerts.json");
        std::fs::create_dir_all(&file).unwrap();
        (dir, file)
    }

    #[test]
    fn an_alert_which_cant_be_saved_isnt_added() {
        let (dir, file) = unwritable("unsaved-add");
        let alerts = Alerts::load(None).unwrap();
        let alerts = Alerts {
            file: Some(file),
            ..alerts
        };

        assert!(alerts.add(alert(1, group())).is_err());
        assert!(alerts.all().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_alert_is_removed_even_if_that_cant_be_saved() {
        let (dir, file) = unwritable("unsaved-remove");
        let alerts = Alerts::load(None).unwrap();
        let fired = alerts.add(alert(1, group())).unwrap().unwrap();
        let alerts = Alerts {
            file: Some(file),
            ..alerts
        };

        // so a fired alert isn't posted again at the next refresh
        assert!(alerts.remove(fired.id));
        assert!(alerts.all().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn alerts_which_cant_be_parsed_are_moved_aside() {
        let dir = std::env::temp_dir().join(format!("price-alerts-cut-{}", std::process::id()));
        let file = dir.join("alerts.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "{\"next_id\":3,\"ale").unwrap();

        let alerts = Alerts::load(Some(file.clone())).unwrap();

        assert!(alerts.all().is_empty());
        assert!(!file.exists());
        let aside: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(aside.len(), 1);
        // and new ones are saved as before
        alerts.add(alert(1, group())).unwrap();
        assert_eq!(Alerts::load(Some(file)).unwrap().all().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn alerts_outlast_a_restart() {
        let dir = std::env::temp_dir().join(format!("price-alerts-{}", std::process::id()));
        let file = dir.join("alerts.json");
        let _ = std::fs::remove_dir_all(&dir);

        let alerts = Alerts::load(Some(file.clone())).unwrap();
        let first = alerts.add(alert(1, group())).unwrap().unwrap();
        let mut second = alerts.add(alert(2, group())).unwrap().unwrap();
        alerts.remove(first.id);
        alerts.confirmed(second.id, 7);
        second.confirmation = Some(7);

        let reloaded = Alerts::load(Some(file.clone())).unwrap();
        assert_eq!(reloaded.all(), std::slice::from_ref(&second));
        // and ids carry on from where they were, so a removed one's isn't reused
        assert!(reloaded.add(alert(1, group())).unwrap().unwrap().id > second.id);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
