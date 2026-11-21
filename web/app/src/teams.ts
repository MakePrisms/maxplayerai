declare const WEB_ANALYTICS_ENABLED: boolean;
/**
 * The Teams landing page (/). The page's script is src/site/teams.js; this entry adds what every page of the site
 * shares: page-view analytics, compiled in on production builds only.
 */
import { startAnalytics } from "./analytics.js";
import { stylesLoaded } from "./site/styled.js";

// Old #market deep links (skill.md, llms.txt, shared URLs) pointed at the board when it sat on the homepage. Checked
// before the page's script runs: that script rewrites the address to the section in view as soon as it starts.
if (location.hash === "#market") location.replace("/market");
else void stylesLoaded().then(() => import("./site/teams.js"));

if (WEB_ANALYTICS_ENABLED) startAnalytics();
