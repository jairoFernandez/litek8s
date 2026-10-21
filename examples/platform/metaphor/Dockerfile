FROM busybox:1.37
COPY index.html /www/index.html
USER 65534
EXPOSE 8080
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
