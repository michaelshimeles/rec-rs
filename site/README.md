# rec-rs site

The download page for Rec. SvelteKit with `adapter-vercel`; the page is prerendered. Run the
commands from this folder:

```sh
npm install
npm run dev       # http://localhost:5173
npm run build
npm run preview   # serves the production build, including /download
```

`npm install` and the build write `node_modules/`, `.svelte-kit/` and `.vercel/` into this
folder. The repo is synced between machines (Mutagen), so run them in a clone or worktree
outside the synced checkout.

- Deploy it as a Vercel project with the Root Directory set to `site`.
- `/download` (`src/routes/download/+server.ts`) answers with a 302 to
  `https://github.com/michaelshimeles/rec-rs/releases/latest/download/Rec.dmg`, which
  `scripts/release-macos.sh` builds. Every GitHub release must include an asset with exactly
  that name.
- The version, URLs, title, description and author live in `src/lib/site.ts`. Update
  `VERSION` with each release. If the site gets another domain, update `SITE_URL` (the
  canonical, Open Graph and JSON-LD tags use it) and the domain in `static/robots.txt` and
  `static/sitemap.xml`.
- `src/lib/assets/og.jpg` is the 1200x630 share image. The page imports it, so its URL has a
  content hash and changes with the image. `/og.jpg` redirects to it for old links. Edit
  `scripts/og/og.html` and run `scripts/og/render.sh` to regenerate it; it needs macOS (for
  `sips`) and Google Chrome. `scripts/og/shot.webp` is the Rec window captured from the app in
  dark appearance.
- The demo and the feature cards are HTML and CSS that recreate the Rec window, not
  screenshots or video. The demo's timeline, cursor path, captions and sounds live in
  `src/lib/demo/script.ts`; its sounds are synthesized with Web Audio in `src/lib/demo/sound.ts`.
- The page uses the system fonts (SF Pro and SF Mono on a Mac) and makes no third-party
  requests in production.
- `src/routes/+layout.ts` adds Vercel Web Analytics, which counts page views without cookies
  and stores no personal data. In production it loads its script and sends page views to a
  per-project path on the site's own domain, which Vercel passes in at build time through the
  `VITE_VERCEL_OBSERVABILITY_*` variables. `npm run dev` loads a debug script from
  `va.vercel-scripts.com` that logs page views to the console and sends nothing. The layout
  calls `inject()` from `@vercel/analytics` because the package's SvelteKit helper imports
  `$app/stores`, which throws in SvelteKit 3. The package's `@sveltejs/kit` peer range also
  stops at Kit 2, so `package.json` overrides it to let a plain `npm install` succeed.
