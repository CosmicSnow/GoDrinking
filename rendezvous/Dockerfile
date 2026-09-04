FROM node:22-bookworm-slim
WORKDIR /app
ENV NODE_ENV=production
ENV PORT=8787
RUN adduser --system --uid 10001 --group godrinking
COPY package.json package-lock.json ./
RUN npm ci --omit=dev
COPY --chown=godrinking:godrinking server.mjs ./
USER godrinking
EXPOSE 8787
CMD ["node", "server.mjs"]