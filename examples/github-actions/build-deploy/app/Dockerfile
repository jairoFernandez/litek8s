FROM golang:1.23-alpine AS build
WORKDIR /src
COPY go.mod main.go ./
ARG VERSION=dev
RUN CGO_ENABLED=0 go build -ldflags "-s -w -X main.version=${VERSION}" -o /app .

FROM alpine:3.20
COPY --from=build /app /app
USER 65534
EXPOSE 8080
ENTRYPOINT ["/app"]
