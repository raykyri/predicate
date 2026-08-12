// Styles for the marketing page shell: the 8-column editorial grid, its type
// ramp, and the light/dark palette. The app replica brings its own stylesheet
// (mockupCss.ts) and does not inherit from here.
export const LANDING_CSS = `
/* Valley Sans (100-900 variable), JetBrains Mono, and DM Sans webfonts, bundled
   under the SIL OFL 1.1 (licenses in site/fonts/). DM Sans and JetBrains Mono are
   the desktop app's UI and code faces; the replica needs them to read as the app. */
@font-face {
  font-family: "Valley Sans";
  src: url("/fonts/ValleySans-Variable.woff2") format("woff2");
  font-style: normal;
  font-weight: 100 900;
  font-display: swap;
}
@font-face {
  font-family: "JetBrains Mono";
  src: url("/fonts/JetBrainsMono-Regular.woff2") format("woff2");
  font-style: normal;
  font-weight: 400;
  font-display: swap;
}
@font-face {
  font-family: "DM Sans";
  src: url("/fonts/DMSans-Variable-Latin.woff2") format("woff2");
  font-style: normal;
  font-weight: 100 1000;
  font-display: swap;
}

:root {
  color-scheme: light dark;
  --bg: #fcfcfc;
  --fg: #2e2e2e;
  --muted: #767676;
  --surface: #f3f3f3;
  --surface-strong: #e8e8e8;
  --line: #2e2e2e;
  --focus: #2e2e2e;
  --gutter: 2.5rem;
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #131313;
    --fg: #e6e6e6;
    --muted: #b3b3b3;
    --surface: #1a1a1a;
    --surface-strong: #292929;
    --line: #e6e6e6;
    --focus: #e6e6e6;
  }
}

* {
  box-sizing: border-box;
}

html {
  scroll-behavior: smooth;
}

body {
  margin: 0;
  padding: 1.5rem;
  background: var(--bg);
  color: var(--fg);
  font-family: "Valley Sans", system-ui, -apple-system, BlinkMacSystemFont, "Helvetica Neue", Arial, sans-serif;
  font-size: 18px;
  line-height: 1.4;
  -webkit-font-smoothing: antialiased;
  text-rendering: optimizeLegibility;
}

a {
  color: inherit;
}

a:focus-visible,
summary:focus-visible {
  outline: 2px solid var(--focus);
  outline-offset: 3px;
}

.skip-link {
  position: absolute;
  left: -9999px;
  top: 0;
  padding: 0.5rem 0.75rem;
  background: var(--bg);
  color: var(--fg);
  font-size: 15px;
  text-decoration: none;
}

.skip-link:focus {
  left: 1.5rem;
  z-index: 10;
  outline: 2px solid var(--focus);
  outline-offset: 2px;
}

.page-grid {
  display: grid;
  grid-template-columns: repeat(8, minmax(0, 1fr));
  column-gap: var(--gutter);
  row-gap: var(--gutter);
  max-width: 1480px;
  margin: 0 auto;
}

.site-header,
.main-grid,
.grid-section {
  display: contents;
}

p,
h1,
h2,
ul {
  margin: 0;
}

code {
  font-family: "JetBrains Mono", "SFMono-Regular", Consolas, monospace;
  font-size: 0.79em;
  line-height: 1;
  background: var(--surface-strong);
  padding: 0.12em 0.34em;
  border-radius: 3px;
}

/* Header */
.brand {
  grid-column: 1 / 4;
  align-self: start;
  display: inline-flex;
  align-items: center;
  width: fit-content;
  font-size: 15px;
  line-height: 22px;
  text-decoration: none;
}

.brand img {
  width: 36px;
  height: 36px;
  border-radius: 8px;
}

.site-nav {
  grid-column: 7 / 9;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 0.15rem;
  font-size: 15px;
  line-height: 22px;
}

.site-nav a,
.footer-links a {
  text-decoration-thickness: 1px;
  text-underline-offset: 0.18em;
  text-decoration-color: transparent;
}

.site-nav a:hover,
.footer-links a:hover {
  text-decoration-color: currentColor;
}

.site-nav .nav-external::after {
  content: " ↗";
}

/* Hero */
.hero-title {
  grid-column: 1 / 6;
  font-size: clamp(2.125rem, 3.35vw, 2.625rem);
  line-height: 0.98;
  font-weight: 400;
  letter-spacing: -0.035em;
  text-wrap: balance;
}

.hero-links {
  grid-column: 1 / 6;
  display: flex;
  flex-wrap: wrap;
  gap: 1rem 2rem;
  align-items: center;
}

.text-link {
  display: inline-flex;
  align-items: center;
  gap: 0.28rem;
  width: fit-content;
  text-decoration: none;
  font-size: 18px;
  line-height: 25px;
}

.text-link::after {
  content: "→";
  display: inline-block;
  transition: transform 150ms ease;
}

.text-link:hover::after {
  transform: translateX(4px);
}

.text-link.secondary::after {
  content: "↗";
}

.subtle-link {
  color: var(--muted);
  font-size: 18px;
  line-height: 25px;
  opacity: 0.6;
}

.intro-copy {
  grid-column: 3 / 7;
  display: flex;
  flex-direction: column;
  gap: 1.35rem;
  font-size: 18px;
  line-height: 25px;
}

.product-shot {
  grid-column: 1 / 9;
  margin: 0;
}

.product-shot figcaption {
  max-width: 46rem;
  margin-top: 0.7rem;
  color: var(--muted);
  font-size: 15px;
  line-height: 22px;
  text-wrap: pretty;
}

/* Editorial sections */
.section-label {
  grid-column: 1 / 3;
  align-self: start;
  font-size: 18px;
  line-height: 25px;
  font-weight: 400;
}

.feature-list,
.faq-list {
  grid-column: 3 / 8;
}

.feature-list {
  list-style: none;
  padding: 0;
  border-top: 1px solid var(--line);
}

.feature-list li {
  display: grid;
  grid-template-columns: minmax(9rem, 1fr) minmax(0, 2fr);
  gap: var(--gutter);
  padding: 0.75rem 0;
  border-bottom: 1px solid var(--line);
  font-size: 18px;
  line-height: 25px;
}

.feature-list strong {
  font-weight: 400;
}

.feature-copy {
  color: var(--muted);
  text-wrap: pretty;
}

.faq-list {
  border-top: 1px solid var(--line);
}

.faq-list details {
  border-bottom: 1px solid var(--line);
}

.faq-list summary {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  padding: 0.75rem 0;
  cursor: pointer;
  list-style: none;
  font-size: 18px;
  line-height: 25px;
}

.faq-list summary::-webkit-details-marker {
  display: none;
}

.faq-list summary::after {
  content: "+";
  flex: 0 0 auto;
  width: 1rem;
  font-size: 24px;
  line-height: 1;
  font-weight: 300;
  text-align: center;
}

.faq-list details[open] summary::after {
  content: "−";
}

.faq-list details p {
  max-width: 46rem;
  text-wrap: pretty;
  padding: 0.25rem 2.5rem 0.9rem 0;
  color: var(--muted);
  font-size: 18px;
  line-height: 25px;
}

.faq-list details p + p {
  padding-top: 0;
}

/* Closing */
.closing {
  grid-column: 3 / 7;
  display: flex;
  flex-wrap: wrap;
  gap: 1rem 2rem;
}

.site-footer {
  grid-column: 1 / 9;
  display: grid;
  grid-template-columns: repeat(8, minmax(0, 1fr));
  column-gap: var(--gutter);
  padding-top: 1.25rem;
  border-top: 1px solid var(--line);
  font-size: 15px;
  line-height: 22px;
}

.footer-mark {
  grid-column: 1 / 3;
  display: flex;
  align-items: center;
  gap: 0.55rem;
}

.footer-mark img {
  width: 18px;
  height: 18px;
  border-radius: 4px;
}

.footer-links {
  grid-column: 3 / 7;
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 0.15rem var(--gutter);
}

.footer-links a {
  width: fit-content;
}

.copyright {
  grid-column: 7 / 9;
  color: var(--muted);
}

@media (min-width: 64rem) {
  body {
    padding: 2rem 3rem;
  }

  :root {
    --gutter: 3rem;
  }
}

@media (max-width: 52rem) {
  .hero-title {
    grid-column: 1 / 8;
  }

  .intro-copy {
    grid-column: 2 / 9;
  }

  .feature-list,
  .faq-list {
    grid-column: 3 / 9;
  }

  .closing {
    grid-column: 3 / 9;
  }

  .feature-list li {
    grid-template-columns: 1fr;
    gap: 0.25rem;
  }
}

@media (max-width: 40rem) {
  :root {
    --gutter: 1rem;
  }

  body {
    padding: 1.25rem;
  }

  .page-grid {
    row-gap: 2.5rem;
  }

  .brand {
    grid-column: 1 / 4;
  }

  .site-nav {
    grid-column: 6 / 9;
  }

  .hero-title,
  .hero-links,
  .intro-copy,
  .product-shot,
  .section-label,
  .feature-list,
  .faq-list,
  .closing {
    grid-column: 1 / 9;
  }

  .footer-mark {
    grid-column: 1 / 4;
    align-items: flex-start;
  }

  .footer-links {
    grid-column: 4 / 7;
    grid-template-columns: 1fr;
  }

  .copyright {
    grid-column: 7 / 9;
  }
}

@media (prefers-reduced-motion: reduce) {
  html {
    scroll-behavior: auto;
  }

  .text-link::after {
    transition: none;
  }
}
`;
