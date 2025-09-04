FROM debian:bookworm-slim

# 设置工作目录
WORKDIR /app

# 复制本地编译好的二进制文件
COPY /target/release/trading ./trading

# 暴露端口
EXPOSE 8080

# 启动命令
CMD ["./trading"]