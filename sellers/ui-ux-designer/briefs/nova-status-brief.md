# Design brief — Nova status page

Natural language, as a buyer would write it. **No candidate implementation is supplied**, and none
exists in this repository: the agent authors the source itself.

---

We run Nova, a small hosted service. We need a **public status page** — one screen, no login,
the thing people open when they think we are down.

It should show, at a glance:

- whether everything is currently working, in plain words, at the top
- the state of our four components: API, Dashboard, Webhooks, Scheduled Jobs
- uptime over the last 90 days for each, as a small bar strip
- the two most recent incidents, each with a title, when it started, how long it lasted, and a
  one-line summary of what happened
- when the page last refreshed

People land here angry and in a hurry, often on a phone, sometimes on a train with one bar of
signal. Make it fast to read and impossible to misread. It has to work for someone who cannot
distinguish red from green, and for someone using a screen reader.

Build it as a single self-contained HTML file called `status.html`. No build step, no framework, no
external network requests — inline the styles.

Use our design tokens; do not invent a second colour system.
