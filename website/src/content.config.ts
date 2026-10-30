import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { defineCollection } from "astro:content";

const REPO = "https://github.com/korora-tech/eloquawk";
const README = fileURLToPath(new URL("../../README.md", import.meta.url));
const PUBLIC = new URL("../public/", import.meta.url);

// Repository files this site serves itself. The Content-Security-Policy only
// allows images from this origin, so the README's screenshots and icon must be
// copies in public/ rather than links to GitHub.
function local(path: string): string | undefined {
  if (path === "data/branding/eloquawk-app-icon.svg") return "/favicon.svg";
  if (path.startsWith("docs/screenshots/")) return `/screenshots/${path.slice("docs/screenshots/".length)}`;
  return undefined;
}

// Where a link relative to the repository root goes on the site: a local copy,
// or the file on GitHub. A missing copy fails the build instead of a broken
// image on the page.
function target(path: string): string {
  const site = local(path);
  if (!site) return `${REPO}/blob/main/${path}`;
  if (!existsSync(fileURLToPath(new URL(`.${site}`, PUBLIC)))) {
    throw new Error(`README.md links ${path}, so copy it to website/public${site}`);
  }
  return site;
}

const RELATIVE = "(?!https?:|#|mailto:|/)";

// The docs page is the repository README, so the two never drift apart. The
// icon and title are dropped because the page supplies its own, GitHub's alert
// markers become a bold label, and links relative to the repository root are
// pointed at the local copies or at GitHub.
function prepare(markdown: string): string {
  return markdown
    .replace(/^(?:<img [^>]*>\s*)?# .*\n+/, "")
    .replace(
      /^> \[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]$/gm,
      (_, kind: string) => `> **${kind[0]}${kind.slice(1).toLowerCase()}:**`,
    )
    .replace(new RegExp(`\\]\\(${RELATIVE}([^)\\s]+)\\)`, "g"), (_, path: string) => `](${target(path)})`)
    .replace(new RegExp(`(<img [^>]*src=")${RELATIVE}([^"]+)"`, "g"), (_, img: string, path: string) => `${img}${target(path)}"`);
}

const docs = defineCollection({
  loader: {
    name: "readme",
    async load({ store, renderMarkdown, generateDigest, watcher }) {
      const sync = async () => {
        const body = prepare(await readFile(README, "utf8"));
        store.set({
          id: "readme",
          data: {},
          body,
          digest: generateDigest(body),
          rendered: await renderMarkdown(body),
        });
      };
      await sync();
      watcher?.add(README);
      watcher?.on("change", (path) => {
        if (path === README) void sync();
      });
    },
  },
});

export const collections = { docs };
