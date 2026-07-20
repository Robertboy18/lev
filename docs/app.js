(() => {
  "use strict";

  const body = document.body;
  const navToggle = document.querySelector("[data-nav-toggle]");
  const navClose = document.querySelector("[data-nav-close]");
  const sectionLinks = [...document.querySelectorAll("[data-section-link]")];
  const sections = [...document.querySelectorAll(".doc-section[id]")];

  function setNavigationOpen(open) {
    body.classList.toggle("nav-open", open);
    navToggle?.setAttribute("aria-expanded", String(open));
    if (navToggle) {
      navToggle.textContent = open ? "Close" : "Contents";
    }
  }

  navToggle?.addEventListener("click", () => {
    setNavigationOpen(!body.classList.contains("nav-open"));
  });
  navClose?.addEventListener("click", () => setNavigationOpen(false));
  sectionLinks.forEach((link) => {
    link.addEventListener("click", () => setNavigationOpen(false));
  });
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      setNavigationOpen(false);
    }
  });

  async function copyText(text, source) {
    if (navigator.clipboard && window.isSecureContext) {
      try {
        await navigator.clipboard.writeText(text);
        return true;
      } catch {
        // Local files can be secure contexts while still denying permission.
      }
    }

    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.opacity = "0";
    document.body.append(textarea);
    textarea.select();
    let copied = false;
    try {
      copied = document.execCommand("copy");
    } catch {
      // A directly opened file can deny legacy clipboard access entirely.
    }
    textarea.remove();
    if (copied) {
      return true;
    }

    const selection = window.getSelection();
    if (!selection) {
      throw new Error("Text selection was unavailable");
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
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy code");
    block.append(button);

    button.addEventListener("click", async () => {
      try {
        const copied = await copyText(code.textContent.trim(), code);
        button.textContent = copied ? "Copied" : "Selected";
      } catch {
        button.textContent = "Select manually";
      }
      window.setTimeout(() => {
        button.textContent = "Copy";
      }, 1600);
    });
  });

  const search = document.querySelector("#doc-search");
  const emptySearch = document.querySelector("[data-search-empty]");
  search?.addEventListener("input", () => {
    const query = search.value.trim().toLowerCase();
    let matches = 0;

    sectionLinks.forEach((link) => {
      const section = document.querySelector(link.getAttribute("href"));
      const searchable = `${link.textContent} ${section?.dataset.title ?? ""}`.toLowerCase();
      const visible = !query || searchable.includes(query);
      link.hidden = !visible;
      matches += Number(visible);
    });

    document.querySelectorAll(".nav-group").forEach((group) => {
      group.hidden = ![...group.querySelectorAll("[data-section-link]")].some(
        (link) => !link.hidden,
      );
    });

    if (emptySearch) {
      emptySearch.hidden = matches > 0;
    }
  });

  if ("IntersectionObserver" in window) {
    const sectionById = new Map(
      sectionLinks.map((link) => [link.getAttribute("href").slice(1), link]),
    );
    const observer = new IntersectionObserver(
      (entries) => {
        const visible = entries
          .filter((entry) => entry.isIntersecting)
          .sort((left, right) => right.intersectionRatio - left.intersectionRatio)[0];
        if (!visible) {
          return;
        }
        sectionLinks.forEach((link) => link.classList.remove("active"));
        sectionById.get(visible.target.id)?.classList.add("active");
      },
      {
        rootMargin: "-15% 0px -70% 0px",
        threshold: [0, 0.1, 0.5],
      },
    );
    sections.forEach((section) => observer.observe(section));
  }
})();
