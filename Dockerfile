FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# 设置工作目录
WORKDIR /app

# 复制本地编译好的二进制文件
COPY /target/release/trading ./trading

# 暴露端口
EXPOSE 8080

# 启动命令
CMD ["./trading"]