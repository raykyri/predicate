// The marketing page at "/". Server-rendered from these components rather than
// hand-maintained HTML, so the page and the app replica it embeds share one
// component tree and one build.
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import AppMockup from "./AppMockup";
import {
  DOWNLOAD_URL,
  FAQS,
  FEATURES,
  GITHUB_URL,
  LATEST_VERSION,
  RELEASES_URL,
  SITE_DESCRIPTION,
  SITE_TITLE,
} from "./content";
import { LANDING_CSS } from "./landingCss";
import { MOCKUP_CSS } from "./mockupCss";

// FAQ answers mark inline code with backticks; render those runs as <code>.
function FaqAnswer({ text }: { text: string }) {
  const parts = text.split("`");
  return (
    <p>
      {parts.map((part, index) =>
        index % 2 === 1 ? <code key={index}>{part}</code> : <React.Fragment key={index}>{part}</React.Fragment>,
      )}
    </p>
  );
}

function SiteHeader() {
  return (
    <header className="site-header">
      <a className="brand" href="/" aria-label="qmux home">
        <img src="/logo.png" alt="" width={36} height={36} decoding="async" />
      </a>
      <nav className="site-nav" aria-label="Main navigation">
        <a href="#features-title">Features</a>
        <a href="#faq-title">FAQ</a>
        <a className="nav-external" href={GITHUB_URL}>
          GitHub
        </a>
        <a className="nav-external" href={DOWNLOAD_URL}>
          Download
        </a>
      </nav>
    </header>
  );
}

function Hero() {
  return (
    <section className="grid-section" aria-labelledby="hero-title">
      <h1 className="hero-title" id="hero-title">
        Visual queueing for your coding agents.
      </h1>
      <div className="hero-links">
        <a className="text-link secondary" href={GITHUB_URL}>
          View on GitHub
        </a>
        <a className="text-link secondary" href={DOWNLOAD_URL}>
          Download v{LATEST_VERSION}
        </a>
        <span className="subtle-link">MIT License</span>
      </div>
      <div className="intro-copy">
        <p>
          Qmux is a terminal for coding agents, with first-class support for visual queueing and
          agent orchestration.
        </p>
        <p>
          Sequence dozens of tasks for Claude, Codex, Grok, or OpenCode. Multiplex agents to check
          each others&rsquo; work.
        </p>
      </div>
      <figure className="product-shot">
        <AppMockup labelledBy="product-shot-caption" />
        <figcaption id="product-shot-caption">
          qmux running a Codex agent over the Porffor JavaScript engine: projects and their
          agents on the left, the live terminal in the middle, the rendered transcript and turn
          queue on the right.
        </figcaption>
      </figure>
    </section>
  );
}

function Features() {
  return (
    <section className="grid-section" id="features" aria-labelledby="features-title">
      <h2 className="section-label" id="features-title">
        Features
      </h2>
      <ul className="feature-list">
        {FEATURES.map((feature) => (
          <li key={feature.name}>
            <strong>{feature.name}</strong>
            <span className="feature-copy">{feature.copy}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

function Faq() {
  return (
    <section className="grid-section" id="faq" aria-labelledby="faq-title">
      <h2 className="section-label" id="faq-title">
        FAQ
      </h2>
      <div className="faq-list">
        {FAQS.map((entry) => (
          <details key={entry.question}>
            <summary>{entry.question}</summary>
            {entry.answers.map((answer, index) => (
              <FaqAnswer key={index} text={answer} />
            ))}
          </details>
        ))}
      </div>
    </section>
  );
}

function SiteFooter() {
  return (
    <footer className="site-footer">
      <div className="footer-mark">
        <img src="/logo.png" alt="" width={18} height={18} decoding="async" />
        qmux
      </div>
      <div className="footer-links">
        <a href={GITHUB_URL}>GitHub</a>
        <a href={`${GITHUB_URL}/issues`}>Issues</a>
        <a href={`${GITHUB_URL}/commits/main`}>Changelog</a>
        <a href={`${GITHUB_URL}/blob/main/LICENSE`}>License (MIT)</a>
      </div>
      <span className="copyright">&copy; 2026 qmux</span>
    </footer>
  );
}

export function LandingPage() {
  return (
    <div className="page-grid">
      <SiteHeader />
      <main className="main-grid" id="main">
        <Hero />
        <Features />
        <Faq />
        <div className="closing">
          <a className="text-link secondary" href={GITHUB_URL}>
            View on GitHub
          </a>
          <a className="text-link secondary" href={RELEASES_URL}>
            Download
          </a>
        </div>
      </main>
      <SiteFooter />
    </div>
  );
}

function LandingDocument({ origin }: { origin: string }) {
  // Scrapers resolve og:image/og:url against nothing, so they have to be
  // absolute; the origin comes from QMUX_PUBLIC_ORIGIN in production.
  const canonical = `${origin}/`;
  return (
    <html lang="en">
      <head>
        <meta charSet="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <meta name="theme-color" media="(prefers-color-scheme: light)" content="#fcfcfc" />
        <meta name="theme-color" media="(prefers-color-scheme: dark)" content="#131313" />
        <title>{SITE_TITLE}</title>
        <meta name="description" content={SITE_DESCRIPTION} />
        <link rel="canonical" href={canonical} />
        <meta property="og:type" content="website" />
        <meta property="og:site_name" content="qmux" />
        <meta property="og:title" content={SITE_TITLE} />
        <meta property="og:description" content={SITE_DESCRIPTION} />
        <meta property="og:url" content={canonical} />
        <meta property="og:image" content={`${origin}/qmux.png`} />
        <meta name="twitter:card" content="summary_large_image" />
        <link rel="icon" type="image/png" href="/logo.png" />
        <link
          rel="preload"
          href="/fonts/ValleySans-Variable.woff2"
          as="font"
          type="font/woff2"
          crossOrigin="anonymous"
        />
        <link
          rel="preload"
          href="/fonts/DMSans-Variable-Latin.woff2"
          as="font"
          type="font/woff2"
          crossOrigin="anonymous"
        />
        {/* The replica's terminal is monospace from first paint; without the
            preload it reflows once the webfont lands. */}
        <link
          rel="preload"
          href="/fonts/JetBrainsMono-Regular.woff2"
          as="font"
          type="font/woff2"
          crossOrigin="anonymous"
        />
        <style>{`${LANDING_CSS}${MOCKUP_CSS}`}</style>
        {/* Progressive enhancement only: the replica is complete without it. */}
        <script src="/mockup.js" defer />
      </head>
      <body>
        <a className="skip-link" href="#main">
          Skip to content
        </a>
        <LandingPage />
      </body>
    </html>
  );
}

// The page is static apart from the origin, so it is rendered once and served
// from memory rather than re-running React for every request.
let cached: { origin: string; html: string } | null = null;

export function renderLandingPage(origin: string) {
  if (cached?.origin === origin) {
    return cached.html;
  }
  const html = `<!doctype html>${renderToStaticMarkup(<LandingDocument origin={origin} />)}`;
  cached = { origin, html };
  return html;
}
