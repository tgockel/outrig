FROM docker.io/library/alpine:latest
RUN apk add --no-cache nodejs npm shadow
RUN npm install -g @modelcontextprotocol/server-filesystem
COPY entry.sh /entry.sh
RUN chmod 0755 /entry.sh
ENTRYPOINT ["/entry.sh"]
