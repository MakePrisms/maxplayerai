# Email images

Header and footer images for the Maxplayer signup emails (Resend templates).
Email clients like Gmail and Outlook ignore web fonts, so the logo, wordmark and
headlines are rendered to PNG from the site's own fonts (`../fonts/`).

The templates load them from `https://www.maxplayer.ai/email/<name>.png`. Don't
rename or remove a file while a template still points at it; add a new name instead.

- `hero-code.png` — code email ("Link your agent")
- `hero-welcome.png` — welcome email ("Welcome to Maxplayer")
- `hero-agents.png` — agent list email ("Your agents")
- `footer.png` — outline wordmark and red/yellow finish stripe, shared by all three
