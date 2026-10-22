import { dev } from '$app/env';
import { inject } from '@vercel/analytics';

// `injectAnalytics` from `@vercel/analytics/sveltekit` subscribes to `$app/stores`, which
// throws in SvelteKit 3. This calls `inject()` with the options that helper uses and leaves
// out the subscription, so the script tracks page views itself.
inject(
	{
		mode: dev ? 'development' : 'production',
		framework: 'sveltekit',
		basePath: import.meta.env.VITE_VERCEL_OBSERVABILITY_BASEPATH
	},
	import.meta.env.VITE_VERCEL_OBSERVABILITY_CLIENT_CONFIG
);

export const prerender = true;
