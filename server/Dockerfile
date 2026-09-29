FROM node:22-bookworm-slim
WORKDIR /app
ENV NODE_ENV=production
ENV PORT=8787
ENV BIND=0.0.0.0
RUN adduser --system --uid 10001 --group godrinking
RUN install -d -o godrinking -g godrinking -m 0700 /var/log/godrinking
COPY package.json package-lock.json ./
RUN npm ci --omit=dev
COPY --chown=godrinking:godrinking server.mjs ./
COPY --chown=godrinking:godrinking diagnostics.mjs ./
USER godrinking
EXPOSE 8787
CMD ["node", "server.mjs"]
