# Email images

Header and footer images for the Maxplayer signup emails (Resend templates).
Email clients like Gmail and Outlook ignore web fonts, so the logo, wordmark and
headlines are rendered to PNG from the site's own fonts (`../fonts/`).

The templates load them from `https://www.maxplayer.ai/email/<name>.png`. Don't
rename or remove a file while a template still points at it; add a new name instead.

- `hero-code-v2.png` — code email ("Link your agent")
- `hero-welcome-v2.png` — welcome email ("Welcome to Maxplayer")
- `footer-wordmark.png` — outline wordmark, shared by both
- `social-x.png`, `social-github.png` — footer social icons (20px, 2x), shared by both
- `hero-code-v2-light.png`, `hero-welcome-v2-light.png`, `footer-wordmark-light.png` — light-mode
  versions. The templates show these by default and swap to the dark ones in clients that
  support dark-mode CSS (`prefers-color-scheme`).
- `social-x-t.png`, `social-github-t.png` — the social icons on a transparent background, so
  one file works on light and dark. The older `social-*.png` stay until no template uses them.
