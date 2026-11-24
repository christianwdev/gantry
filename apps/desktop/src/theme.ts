/**
 * Gantry styles key off `data-theme` on <html> exclusively. For now the theme
 * simply follows the OS `prefers-color-scheme` setting.
 */
export type Theme = "light" | "dark";

const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");

function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
}

/** Sets the initial theme and keeps it in sync with the OS preference. */
export function initTheme(): void {
  applyTheme(darkQuery.matches ? "dark" : "light");
  darkQuery.addEventListener("change", (event) => {
    applyTheme(event.matches ? "dark" : "light");
  });
}
