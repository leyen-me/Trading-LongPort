# ======== 阶段 1: 构建阶段（使用 nightly） ========
FROM rustlang/nightly:latest AS builder

WORKDIR /app

# 启用不稳定功能
ENV RUSTFLAGS="-C target-cpu=generic"

# 复制依赖文件
COPY Cargo.toml Cargo.lock ./

# 创建临时 src 以进行依赖预下载
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

# 使用 nightly cargo 下载依赖（支持 unstable features）
RUN cargo build --release || true

# 复制真实源码
COPY src ./src

# 清理并重新构建正式版本
RUN rm -f target/release/deps/*main* || true
RUN cargo build --release --bin main

# ======== 阶段 2: 运行阶段 ========
FROM debian:bookworm-slim AS runtime

RUN adduser --disabled-password --gecos '' appuser && \
    mkdir /app && \
    chown appuser:appuser /app

WORKDIR /app

RUN apt-get update && \
    apt-get install -y ca-certificates tzdata && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder --chown=appuser:appuser /app/target/release/main .

USER appuser

EXPOSE 8080

CMD ["./main"]