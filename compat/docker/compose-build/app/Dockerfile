FROM python:3.12-alpine
ARG APP_VERSION=dev
ENV APP_VERSION=$APP_VERSION PYTHONUNBUFFERED=1
WORKDIR /srv/app
RUN mkdir vendor && echo "from-image" > vendor/marker
COPY server.py .
EXPOSE 8000
CMD ["python", "server.py"]
