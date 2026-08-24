FROM python:3.14-alpine

COPY deploy/observability/discord-alert-bridge.py /usr/local/bin/discord-alert-bridge.py

USER nobody
EXPOSE 9880
ENTRYPOINT ["python3", "-u", "/usr/local/bin/discord-alert-bridge.py"]
