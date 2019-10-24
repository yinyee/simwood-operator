# Build static Rust binary - to save time on rebuilds, we create a new project
# and build all the dependencies (which change rarely) before copying in the
# source and building the final binary
FROM clux/muslrust AS build
RUN USER=root cargo new simwood-operator
WORKDIR /volume/simwood-operator
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release
RUN cargo clean -p simwood-operator --release
COPY src/* src/
RUN cargo build --release

# Build a container containing that Rust binary
FROM scratch
COPY --from=build /volume/simwood-operator/target/x86_64-unknown-linux-musl/release/simwood-operator ./
CMD ["/simwood-operator"]
