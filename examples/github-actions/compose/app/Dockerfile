FROM python:3.12-alpine
WORKDIR /app
COPY app.py .
EXPOSE 8000
HEALTHCHECK --interval=1s --timeout=2s --retries=30 CMD wget -qO- http://127.0.0.1:8000/health || exit 1
CMD ["python", "-u", "app.py"]
