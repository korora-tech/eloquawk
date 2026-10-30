# eloquawk.korora.tech

The Eloquawk website: an [Astro](https://astro.build) static site served by Cloudflare Workers static assets.

- `src/pages/index.astro`: the home page, with the demo-mode screenshots from `public/screenshots/`.
- `src/pages/docs.astro`: the repository `README.md`, rendered at build time by the loader in `src/content.config.ts`. Edit the README, not this site, to change the docs.

## Screenshots and icon

The Content-Security-Policy only allows images from the site itself, so the site serves copies:

- `public/screenshots/` holds copies of `docs/screenshots/*.webp`.
- `public/favicon.svg` is a copy of `data/branding/eloquawk-app-icon.svg`, and is also the logo in the header.

When a screenshot or the icon changes, copy it again. The docs loader points the README's `docs/screenshots/` and `data/branding/eloquawk-app-icon.svg` links at these copies, and the build fails if the README links a screenshot that has not been copied. Other links relative to the repository root go to the file on GitHub.

## Develop

Needs Node 22.23.4 or newer (`.node-version` pins 24).

```sh
npm ci
npm run dev       # http://localhost:4321
npm run check     # type-check
npm run build     # writes the Build Output to .cloudflare/output
npm run preview   # serve the site locally with cf dev, as Cloudflare would
```

`astro preview` after a build also serves the built output in workerd. On NixOS the `workerd` binary from npm needs patching (or `nix-ld`) before `npm run check` and `astro preview` can start it.

## Deploy

`.github/workflows/website.yml` builds every pull request that touches `website/` or `README.md`, and runs `cf deploy` on `main`. The deploy job runs in the `production` environment and logs in to Infisical over OIDC as the Korora Tech CI identity, which reads the Korora Tech Cloudflare token (`CLOUDFLARE_API_TOKEN`). The identity, this repository's `production` environment and its `INFISICAL_MACHINE_IDENTITY_ID` secret are managed in [korora-tech/korora-tech](https://github.com/korora-tech/korora-tech).

`cloudflare.config.ts` names the account and attaches the Worker to `eloquawk.korora.tech` as a custom domain, so Cloudflare creates the DNS record and certificate on first deploy. The `korora.tech` zone must be in the same account.

Response headers (CSP, caching for hashed assets) live in `public/_headers`.
