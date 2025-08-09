# ======== 阶段 1: 构建阶段 ========
FROM rust:1.78-slim AS builder

# 设置工作目录
WORKDIR /app

# 可选：启用更快的链接（适用于生产）
ENV RUSTFLAGS="-C target-cpu=generic"

# 先复制 Cargo 文件（利用 Docker 缓存）
COPY Cargo.toml Cargo.lock ./

# 创建一个占位源文件，避免构建失败
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

# 预先下载依赖（如果 Cargo.toml 不变，这步不会重复）
RUN cargo build --release || true

# 复制实际源码
COPY src ./src

# 清理之前的假构建并重新构建正式版本
RUN rm target/release/deps/* || true
RUN cargo build --release --bin main

# ======== 阶段 2: 运行阶段 ========
FROM debian:bookworm-slim AS runtime

# 设置非 root 用户（安全最佳实践）
RUN adduser --disabled-password --gecos '' appuser && \
    mkdir /app && \
    chown appuser:appuser /app

WORKDIR /app

# 安装运行时依赖（如 ca-certificates）
RUN apt-get update && \
    apt-get install -y ca-certificates tzdata && \
    rm -rf /var/lib/apt/lists/*

# 从构建阶段复制可执行文件
COPY --from=builder --chown=appuser:appuser /app/target/release/main .

# 切换到非 root 用户
USER appuser

# 暴露服务端口
EXPOSE 8080

# 启动命令
CMD ["./main"]