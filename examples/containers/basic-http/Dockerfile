FROM busybox:1.36.1

WORKDIR /srv

COPY index.html /srv/index.html
COPY health /srv/health

EXPOSE 8080

HEALTHCHECK --interval=5s --timeout=2s --retries=3 \
	CMD wget -q -O - http://127.0.0.1:8080/health >/dev/null || exit 1

CMD ["busybox", "httpd", "-f", "-p", "8080", "-h", "/srv"]
