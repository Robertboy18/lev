(() => {
  "use strict";

  const body = document.body;
  const navToggle = document.querySelector("[data-nav-toggle]");
  const navClose = document.querySelector("[data-nav-close]");

  function setNavigationOpen(open) {
    body.classList.toggle("nav-open", open);
    navToggle?.setAttribute("aria-expanded", String(open));
    if (navToggle) {
      navToggle.textContent = open ? "Close" : "Menu";
    }
  }

  navToggle?.addEventListener("click", () => {
    setNavigationOpen(!body.classList.contains("nav-open"));
  });
  navClose?.addEventListener("click", () => setNavigationOpen(false));
  document.querySelectorAll(".section-nav a").forEach((link) => {
    link.addEventListener("click", () => setNavigationOpen(false));
  });
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      setNavigationOpen(false);
    }
  });

  const currentPage = body.dataset.page;
  if (currentPage) {
    document
      .querySelector(`[data-page-link="${CSS.escape(currentPage)}"]`)
      ?.setAttribute("aria-current", "page");
  }

  document.querySelectorAll(".command-block code").forEach((code) => {
    const hasLanguage = [...code.classList].some((name) =>
      name.startsWith("language-"),
    );
    if (!hasLanguage) {
      const output = code.closest(".command-block")?.classList.contains("output-block");
      code.classList.add(output ? "language-plaintext" : "language-bash");
    }
    window.hljs?.highlightElement(code);
  });

  async function copyText(text, source) {
    if (navigator.clipboard && window.isSecureContext) {
      try {
        await navigator.clipboard.writeText(text);
        return true;
      } catch {
        // Fall through to selection for local previews with denied clipboard access.
      }
    }
    const selection = window.getSelection();
    if (!selection) {
      return false;
    }
    const range = document.createRange();
    range.selectNodeContents(source);
    selection.removeAllRanges();
    selection.addRange(range);
    return false;
  }

  document.querySelectorAll(".command-block").forEach((block) => {
    const code = block.querySelector("code");
    if (!code) {
      return;
    }
    const button = document.createElement("button");
    button.className = "copy-button";
    button.type = "button";
    button.setAttribute("aria-label", "Copy this code");
    button.title = "Copy";
    button.append(document.createElement("span"));
    block.append(button);
    button.addEventListener("click", async () => {
      const copied = await copyText(code.textContent.trim(), code);
      button.classList.add(copied ? "copied" : "selected");
      button.setAttribute("aria-label", copied ? "Copied" : "Code selected");
      button.title = copied ? "Copied" : "Code selected";
      window.setTimeout(() => {
        button.classList.remove("copied", "selected");
        button.setAttribute("aria-label", "Copy this code");
        button.title = "Copy";
      }, 1400);
    });
  });

  const scrollRegions = [
    ...document.querySelectorAll(".command-block pre, .table-wrap, .plot"),
  ];
  function updateScrollableRegions() {
    scrollRegions.forEach((region) => {
      if (region.scrollWidth > region.clientWidth + 1) {
        region.tabIndex = 0;
      } else {
        region.removeAttribute("tabindex");
      }
    });
  }
  updateScrollableRegions();
  window.addEventListener("resize", updateScrollableRegions);

  const tocLinks = [...document.querySelectorAll(".on-this-page a")];
  const sections = tocLinks
    .map((link) => document.querySelector(link.getAttribute("href")))
    .filter(Boolean);
  if ("IntersectionObserver" in window && sections.length) {
    const linksById = new Map(
      tocLinks.map((link) => [link.getAttribute("href").slice(1), link]),
    );
    const observer = new IntersectionObserver(
      (entries) => {
        const visible = entries
          .filter((entry) => entry.isIntersecting)
          .sort((left, right) => right.intersectionRatio - left.intersectionRatio)[0];
        if (!visible) {
          return;
        }
        tocLinks.forEach((link) => link.classList.remove("active"));
        linksById.get(visible.target.id)?.classList.add("active");
      },
      { rootMargin: "-12% 0px -72% 0px", threshold: [0, 0.1, 0.5] },
    );
    sections.forEach((section) => observer.observe(section));
  }
})();
