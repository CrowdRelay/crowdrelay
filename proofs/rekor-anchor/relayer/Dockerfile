FROM node:22-alpine
WORKDIR /app
ENV NODE_ENV=production
COPY package.json ./
COPY index.mjs proof-utils.mjs ./
RUN chown -R node:node /app
USER node
EXPOSE 8081
CMD ["node", "index.mjs"]
