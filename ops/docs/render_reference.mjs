// Prints the system-reference HTML to a dark-mode PDF.
//
// Uses the Chromium that `crowdrelay-agents` already installs for Playwright.
// Nothing else on this machine can render CSS to PDF — no pandoc, no
// wkhtmltopdf, no weasyprint, no Chrome — so the Reddit automation's browser is
// the renderer. Playwright is resolved from the sibling checkout rather than
// adding a Node dependency to this repo, which has none.
//
// `printBackground` is the whole point: without it Chromium drops every
// background colour and a dark-mode document prints as black text on white.

import { createRequire } from "node:module";
import { existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";

const REPO = resolve(import.meta.dirname, "../..");
const AGENTS = resolve(REPO, "../crowdrelay-agents");
const INPUT = resolve(REPO, "ops/docs/build/system-reference.html");
const OUTPUT = process.env.REFERENCE_PDF
  ? resolve(process.env.REFERENCE_PDF)
  : resolve(REPO, "../CrowdRelay-System-Reference.pdf");

if (!existsSync(INPUT)) {
  console.error(`missing ${INPUT} — run: python3 ops/docs/build_reference.py`);
  process.exit(1);
}
if (!existsSync(resolve(AGENTS, "node_modules/playwright"))) {
  console.error(
    `no Playwright in ${AGENTS}. That checkout supplies the only Chromium on ` +
      `this machine; install its dependencies first.`,
  );
  process.exit(1);
}

const require = createRequire(resolve(AGENTS, "package.json"));
const { chromium } = require("playwright");

const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  // `file://` so the single self-contained HTML loads with no server.
  await page.goto(`file://${INPUT}`, { waitUntil: "load" });
  // Chromium only honours a dark page for print when the media type says so.
  // The stylesheet sets every colour explicitly; this keeps
  // `prefers-color-scheme` consistent with them.
  await page.emulateMedia({ media: "print", colorScheme: "dark" });
  await mkdir(dirname(OUTPUT), { recursive: true });
  await page.pdf({
    path: OUTPUT,
    format: "A4",
    printBackground: true,
    displayHeaderFooter: true,
    // Zero everywhere the background must reach; a bottom strip for the
    // footer, which paints itself dark so the page has no white edge at all.
    margin: { top: "0mm", bottom: "9mm", left: "0mm", right: "0mm" },
    headerTemplate: "<div></div>",
    footerTemplate:
      '<div style="width:100%;height:9mm;box-sizing:border-box;' +
      "background:#0d1117;font-family:-apple-system,system-ui,sans-serif;" +
      "font-size:7pt;color:#6e7d8f;padding:2mm 14mm 0;display:flex;" +
      'justify-content:space-between;">' +
      "<span>CrowdRelay — internal technical reference</span>" +
      '<span class="pageNumber"></span></div>',
  });
  console.log(`REFERENCE_PDF=${OUTPUT}`);
} finally {
  await browser.close();
}
