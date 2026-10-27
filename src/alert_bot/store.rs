//! The alerts waiting to fire, in memory and in a JSON file, so that they outlast restarts and
//! deploys. Each change is written to the file before it's kept, so the two always agree.

use crate::prices::Source;
use anyhow::{Context, Result};
use oc_bots_sdk::types::{CanisterId, Chat, InstallationLocation, TimestampMillis, UserId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

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
}

pub struct Alerts {
    file: Option<PathBuf>,
    saved: Mutex<Saved>,
}

impl Alerts {
    /// The alerts in `file`, which each change is written to. Without a file they're only kept in
    /// memory, eg. in tests.
    pub fn load(file: Option<PathBuf>) -> Result<Self> {
        let saved = match &file {
            Some(file) if file.exists() => {
                let json = std::fs::read(file)
                    .with_context(|| format!("Failed to read the alerts in {}", file.display()))?;
                serde_json::from_slice(&json)
                    .with_context(|| format!("Failed to parse the alerts in {}", file.display()))?
            }
            _ => Saved::default(),
        };
        Ok(Alerts {
            file,
            saved: Mutex::new(saved),
        })
    }

    /// Adds the alert, giving it an id, unless its user has as many as they can have in its chat
    pub fn add(&self, mut alert: Alert) -> Result<Option<Alert>> {
        self.change(|saved| {
            let in_chat = saved
                .alerts
                .iter()
                .filter(|a| a.user == alert.user && a.chat == alert.chat)
                .count();
            if in_chat >= MAX_PER_USER_PER_CHAT {
                return None;
            }
            saved.next_id += 1;
            alert.id = saved.next_id;
            saved.alerts.push(alert.clone());
            Some(alert)
        })
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

    /// Removes the user's `n`th alert in the chat, counting from 1, as `/alerts` numbers them
    pub fn remove_nth(&self, user: UserId, chat: Chat, n: usize) -> Result<Option<Alert>> {
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
    pub fn remove(&self, id: u64) -> Result<bool> {
        self.change(|saved| {
            let before = saved.alerts.len();
            saved.alerts.retain(|a| a.id != id);
            saved.alerts.len() < before
        })
    }

    /// Removes every alert in the chats the bot was uninstalled from: a group, a direct chat, or
    /// all a community's channels. Returns how many.
    pub fn remove_at(&self, location: InstallationLocation) -> Result<usize> {
        self.change(|saved| {
            let before = saved.alerts.len();
            saved
                .alerts
                .retain(|a| InstallationLocation::from(a.chat) != location);
            before - saved.alerts.len()
        })
    }

    pub fn all(&self) -> Vec<Alert> {
        self.saved.lock().unwrap().alerts.clone()
    }

    // Changes a copy, writes it to the file, then keeps it, so a failure to save changes nothing
    fn change<T>(&self, f: impl FnOnce(&mut Saved) -> T) -> Result<T> {
        let mut saved = self.saved.lock().unwrap();
        let mut changed = saved.clone();
        let result = f(&mut changed);
        if let Some(file) = &self.file {
            write(file, &changed)?;
        }
        *saved = changed;
        Ok(result)
    }
}

// Written to a temporary file then renamed over the old one, so a crash mid-write leaves the old
// alerts rather than half the new ones
fn write(file: &PathBuf, saved: &Saved) -> Result<()> {
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    let temporary = file.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(saved)?)
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

        assert_eq!(alerts.remove_nth(id(1), group(), 2).unwrap(), Some(second));
        assert_eq!(alerts.remove_nth(id(1), group(), 2).unwrap(), None);
        assert_eq!(alerts.remove_nth(id(1), group(), 0).unwrap(), None);
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

        let dropped = alerts
            .remove_at(InstallationLocation::Community(community))
            .unwrap();
        assert_eq!(dropped, 2);
        assert_eq!(
            alerts
                .remove_at(InstallationLocation::Group(id(GROUP)))
                .unwrap(),
            1
        );
        assert_eq!(alerts.all()[0].chat, Chat::Group(id(11)));
    }

    #[test]
    fn alerts_outlast_a_restart() {
        let dir = std::env::temp_dir().join(format!("price-alerts-{}", std::process::id()));
        let file = dir.join("alerts.json");
        let _ = std::fs::remove_dir_all(&dir);

        let alerts = Alerts::load(Some(file.clone())).unwrap();
        let first = alerts.add(alert(1, group())).unwrap().unwrap();
        let second = alerts.add(alert(2, group())).unwrap().unwrap();
        alerts.remove(first.id).unwrap();

        let reloaded = Alerts::load(Some(file.clone())).unwrap();
        assert_eq!(reloaded.all(), std::slice::from_ref(&second));
        // and ids carry on from where they were, so a removed one's isn't reused
        assert!(reloaded.add(alert(1, group())).unwrap().unwrap().id > second.id);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
