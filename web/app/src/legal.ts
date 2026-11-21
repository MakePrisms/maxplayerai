declare const WEB_ANALYTICS_ENABLED: boolean;
/**
 * The legal pages (/terms/<date>, /privacy/<date>, /fees/<date>), in the look of / and /market. All the script does
 * is the phone-size menu (src/site/menu.ts) and page-view analytics on production builds. The text is static HTML.
 */
import { startAnalytics } from "./analytics.js";
import { wireMenu } from "./site/menu.js";

wireMenu();
if (WEB_ANALYTICS_ENABLED) startAnalytics();
