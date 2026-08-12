// Select the replay's server-rendered starting frame before the mockup can
// paint. The main enhancement script atomically takes over this class once the
// document is parsed. Reduced-motion and older browsers keep the finished mock.
if (
  !window.matchMedia("(prefers-reduced-motion: reduce)").matches &&
  typeof IntersectionObserver === "function"
) {
  document.documentElement.classList.add("mock-replay-boot");
  // If the deferred enhancement fails to load or initialize, restore the
  // complete static session once loading settles instead of leaving it staged.
  window.addEventListener(
    "load",
    () => document.documentElement.classList.remove("mock-replay-boot"),
    { once: true },
  );
}
