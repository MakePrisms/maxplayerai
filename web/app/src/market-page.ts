declare const WEB_ANALYTICS_ENABLED: boolean;
/**
 * The Market page (/market). The page's script is src/site/market.js, on the data layer src/site/market-data.ts;
 * this entry adds page-view analytics, compiled in on production builds only.
 */
import { startAnalytics } from "./analytics.js";
import { stylesLoaded } from "./site/styled.js";

void stylesLoaded().then(() => import("./site/market.js"));

if (WEB_ANALYTICS_ENABLED) startAnalytics();
