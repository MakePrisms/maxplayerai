# Email images

Header and footer images for the Maxplayer signup emails (Resend templates).
Email clients like Gmail and Outlook ignore web fonts, so the logo is rendered to
PNG (headless Chrome, 2x) from the site's own fonts (`../fonts/`), in the look of
the Teams and Market pages.

The templates load them from `https://www.maxplayer.ai/email/<name>.png`. Don't
rename or remove a file while a template still points at it; add a new name instead.

The templates are light by default and switch to dark with `prefers-color-scheme`.
Gmail ignores that query and inverts the colours itself without swapping images,
so the current logo is a single file on a transparent background with a mid-grey
wordmark that reads on white and on near-black. The headers have no image beyond
that logo: the headings are HTML text, so every client recolours them with the
rest of the email.

- `footer-logo-grey.png` — logo for the header and the footer of both emails
- `social-x-teams.png`, `social-github-teams.png` — footer social icons (20px, 2x, transparent), shared by both

Previous versions, kept until the published templates stop pointing at them:
`hero-code-teams.png`, `hero-code-teams-light.png`, `hero-welcome-teams.png`,
`hero-welcome-teams-light.png`, `footer-logo-teams.png`, `footer-logo-teams-light.png`
(headline baked into the image, with a light and a dark copy of each).
