// The marketing page at "/". Server-rendered from these components rather than
// hand-maintained HTML, so the page and the app replica it embeds share one
// component tree and one build.
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import AppMockup from "./AppMockup";
import { HeroAgents } from "./agentIcons";
import { FEATURES, GITHUB_URL, RELEASES_URL, SITE_DESCRIPTION, SITE_TITLE } from "./content";
import { LANDING_CSS } from "./landingCss";
import { MOCKUP_CSS } from "./mockupCss";

function SiteHeader() {
  return (
    <header className="site-header">
      <a className="brand" href="/" aria-label="qmux home">
        <img src="/logo.png" alt="" width={36} height={36} decoding="async" />
      </a>
      <nav className="site-nav" aria-label="Main navigation">
        <a href="#features">Features</a>
        <a className="nav-external" href={GITHUB_URL}>
          GitHub
        </a>
        <a className="nav-external" href={RELEASES_URL}>
          Download
        </a>
        <span className="subtle-link">MIT License</span>
      </nav>
    </header>
  );
}

function Hero() {
  return (
    <section className="grid-section" aria-labelledby="hero-title">
      <div className="hero-lead">
        <h1 className="hero-title" id="hero-title">
          Your terminal, extended
        </h1>
        <HeroAgents />
      </div>
      <div className="intro-copy">
        <p>
          A simpler approach to coding agent orchestration.
        </p>
        <p>
          qmux powers up the terminal with artifacts, visual transcripts, 
          cross-agent queues, and more.
        </p>
      </div>
      <figure className="product-shot">
        <AppMockup />
      </figure>
    </section>
  );
}

function Features() {
  return (
    <section className="grid-section" id="features" aria-label="Features">
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

function SiteFooter() {
  return (
    <footer className="site-footer">
      <div className="footer-links">
        <a className="text-link secondary" href={GITHUB_URL}>
          View on GitHub
        </a>
        <a className="text-link secondary" href={RELEASES_URL}>
          Download
        </a>
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
        {/* This small blocking bootstrap selects the replay's initial state
            before the mockup can paint. The deferred script takes ownership of
            that state; either script can fail without hiding the static mock. */}
        <script src="/mockup-boot.js" />
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
