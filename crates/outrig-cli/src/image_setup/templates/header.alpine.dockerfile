FROM docker.io/library/{IMAGE}

RUN apk add --no-cache ca-certificates curl git build-base bash
