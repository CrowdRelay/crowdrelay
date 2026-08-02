FROM node:22-alpine
WORKDIR /app
ENV NODE_ENV=production
COPY package.json ./
RUN npm install --omit=dev --ignore-scripts --no-audit --fund=false \
    && npm cache clean --force
COPY index.mjs proof-utils.mjs ./
USER node
EXPOSE 8081
CMD ["node", "index.mjs"]
