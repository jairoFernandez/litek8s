# syntax=docker/dockerfile:1
# Multi-stage build with BuildKit syntax the classic builder lacks:
# RUN --mount=type=cache, COPY --chmod, heredocs, TARGETARCH.
ARG ALPINE=3.20
FROM alpine:${ALPINE} AS base
RUN apk add --no-cache ca-certificates

FROM base AS build
ARG GREETING=hello
ARG TARGETARCH
WORKDIR /src
COPY --chmod=755 app.sh ./
COPY data/ ./data/
RUN --mount=type=cache,target=/var/cache/build \
    echo "built $GREETING for $TARGETARCH" > /src/out.txt && date > /var/cache/build/stamp && ls /src/data > /src/files.txt
RUN <<EOF
echo heredoc-line-1 >> /src/out.txt
echo heredoc-line-2 >> /src/out.txt
EOF

FROM base AS unused
RUN false

FROM base
LABEL org.example.test=multistage
ENV APP_HOME=/app
WORKDIR $APP_HOME
COPY --from=build /src/ ./
RUN adduser -D -u 1000 app && chown -R app /app
USER app
EXPOSE 8080
HEALTHCHECK --interval=2s CMD test -f /app/out.txt
CMD ["./app.sh"]
