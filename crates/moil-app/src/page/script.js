"use strict";
// The only script a page runs. It turns clicks and form submissions into
// messages for the app; the app decides what they mean.
(() => {
  const post = (message) => window.ipc.postMessage(JSON.stringify(message));

  document.addEventListener("click", (event) => {
    // Links within the page scroll to their section.
    const anchor = event.target.closest('a[href^="#"]');
    if (anchor) {
      event.preventDefault();
      const target = document.getElementById(anchor.getAttribute("href").slice(1));
      if (target) target.scrollIntoView({ behavior: "smooth", block: "start" });
      return;
    }
    const control = event.target.closest("button[data-action]");
    if (!control || control.disabled) return;
    event.preventDefault();
    const message = { type: control.dataset.action };
    if (control.dataset.hash) message.hash = control.dataset.hash;
    // Actions that take a moment disable every button until the app
    // answers by showing the next page.
    if (control.dataset.busy) {
      for (const button of document.querySelectorAll("button")) button.disabled = true;
      control.textContent = control.dataset.busy;
    }
    post(message);
  });

  document.addEventListener("submit", (event) => {
    event.preventDefault();
    const form = event.target;
    const message = { type: form.dataset.action };
    for (const field of form.elements) {
      if (field.name) message[field.name] = field.value;
    }
    for (const button of form.querySelectorAll("button")) button.disabled = true;
    post(message);
  });

  // Countdowns tick down from their `data-countdown` seconds, written as
  // the app writes waits: "12s", "2 min".
  for (const element of document.querySelectorAll("[data-countdown]")) {
    const end = Date.now() + Number(element.dataset.countdown) * 1000;
    const tick = () => {
      const left = Math.max(0, Math.ceil((end - Date.now()) / 1000));
      element.textContent = left < 60 ? `${left}s` : `${Math.ceil(left / 60)} min`;
      if (left > 0) setTimeout(tick, 1000);
    };
    tick();
  }

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") post({ type: "close" });
  });
})();
