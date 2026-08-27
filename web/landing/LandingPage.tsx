// The marketing page at "/". Server-rendered from these components rather than
// hand-maintained HTML, so the page and the app replica it embeds share one
// component tree and one build.
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import AppMockup, { MOCKUP_FEATURES, RESEARCH_MOCKUP_FEATURES } from "./AppMockup";
import FeatureMiniMockups from "./MiniMockups";
import { HeroAgents } from "./agentIcons";
import { FEATURES, GITHUB_URL, RELEASES_URL, SITE_DESCRIPTION, SITE_TITLE } from "./content";
import { DEFAULT_SESSION_ID } from "./mockupData";
import { LANDING_CSS } from "./landingCss";
import { MOCKUP_CSS } from "./mockupCss";

const HERO_TITLE_PHRASES = [
  "long-running agents",
  "live artifacts",
  "vertical tabs",
  "worktrees",
  "reading the transcript",
  "architecture diagrams",
  "multiplexing work",
];

const SECONDARY_MOCKUP_FEATURES = MOCKUP_FEATURES.filter((feature) => feature !== "replay");

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
          <span className="hero-title-visual" aria-hidden="true">
            <span className="hero-title-prefix">All-in-one terminal for </span>
            <span className="hero-title-rotator">
              {HERO_TITLE_PHRASES.map((phrase) => (
                <span className="hero-title-phrase" key={phrase}>
                  {phrase}
                </span>
              ))}
            </span>
          </span>
          <span className="visually-hidden">All-in-one terminal for long-running agents</span>
        </h1>
        <div className="intro-copy">
          <p>
            qmux is a terminal for CLI agents with visual transcripts, artifacts,
            cross-agent queues, and more.
          </p>
        </div>
        <HeroAgents />
      </div>
      <figure className="product-shot">
        <AppMockup />
        <p className="product-thesis">Your terminal, powered up.</p>
        <p className="product-thesis-description">
          Rapid iteration, long running workflows, or juggling lots of agents? We&apos;ve got you
          covered.
        </p>
        <FeatureMiniMockups />
        <div className="secondary-product-intro">
          <p className="secondary-product-heading">Choose your own adventure.</p>
          <p className="product-thesis-description">
            Use your terminal agents like a desktop app, or switch modes when you need it.
          </p>
        </div>
        <div className="secondary-product-shot">
          <AppMockup
            features={SECONDARY_MOCKUP_FEATURES}
            initialSidebarCollapsed
            initialTranscriptExpanded
          />
        </div>
        <div className="research-product-intro">
          <p className="research-product-heading">Research in the same window.</p>
          <p className="product-thesis-description">
            Flip the sidebar to Research to ask a question, branch the answer where it gets
            interesting, and keep the notes and links that led there.
          </p>
        </div>
        {/* The third replica opens in research mode. It lists every terminal tab
            but ships only the default session's scrollback: the mode toggle here
            demonstrates the swap, not session switching. */}
        <div className="research-product-shot">
          <AppMockup
            features={RESEARCH_MOCKUP_FEATURES}
            initialSidebarMode="research"
            sessionIds={[DEFAULT_SESSION_ID]}
          />
        </div>
        <div className="feature-product-intro">
          <p className="feature-product-heading">Dozens of hand-picked features.</p>
          <p className="product-thesis-description">
            Some useful every day, others matter only when you really need them.
          </p>
        </div>
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
