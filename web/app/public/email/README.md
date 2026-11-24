# Email images

Header and footer images for the Maxplayer signup emails (Resend templates).
Email clients like Gmail and Outlook ignore web fonts, so the logo and headlines
are rendered to PNG (headless Chrome, 2x) from the site's own fonts (`../fonts/`),
in the look of the Teams and Market pages.

The templates load them from `https://www.maxplayer.ai/email/<name>.png`. Don't
rename or remove a file while a template still points at it; add a new name instead.

The templates are light by default and switch to dark with `prefers-color-scheme`,
so each header and footer logo has a `-light` copy (shown by default) and a dark one.

- `hero-code-teams.png`, `hero-code-teams-light.png` — code email ("Link your agent")
- `hero-welcome-teams.png`, `hero-welcome-teams-light.png` — welcome email ("Welcome to Maxplayer")
- `footer-logo-teams.png`, `footer-logo-teams-light.png` — footer logo, shared by both
- `social-x-teams.png`, `social-github-teams.png` — footer social icons (20px, 2x, transparent), shared by both
