# docker build -t price-service .
FROM rust:1.99.0-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/price_service /usr/local/bin/price_service
ENV PORT=8080
EXPOSE 8080
USER nobody
ENTRYPOINT ["price_service"]
