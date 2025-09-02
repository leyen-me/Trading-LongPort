FROM rust:1.78.0

WORKDIR /app

COPY Cargo.toml Cargo.lock ./

COPY ./src ./src

RUN cargo build --release

COPY release/vwap ./vwap

EXPOSE 8080

CMD ["./vwap"]