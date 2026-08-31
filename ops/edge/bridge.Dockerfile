# Minimal image for the n8n bridge: a single dependency-free Node script.
# The full n8n image is ~1GB and pulls in a workflow engine we never use; this
# is ~50MB and contains only the Node runtime the bridge needs.
FROM node:22-alpine

WORKDIR /opt/bridge
COPY bridge.js routes.json ./

EXPOSE 8080
CMD ["node", "/opt/bridge/bridge.js"]
