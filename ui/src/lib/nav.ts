// The ten screens of the mockup, in its order, with its icons, plus the v0.2 Inbox. Paths are the real app's routes.
export interface NavItem {
	path: string;
	key: string; // i18n key of the label
	mockup: string; // hash in docs/mockup/index.html
	icon: string; // SVG inner markup from the mockup
	countFromReview?: boolean;
}

export const NAV: NavItem[] = [
	{ path: '/', key: 'nav_dash', mockup: 'dashboard', icon: '<path d="M3 13h8V3H3zM13 21h8V11h-8zM3 21h8v-6H3zM13 3v6h8V3z"/>' },
	{ path: '/biomarkers', key: 'nav_bio', mockup: 'biomarker', icon: '<path d="M3 3v18h18"/><path d="m7 15 4-5 3 3 5-6"/>' },
	{ path: '/timeline', key: 'nav_tl', mockup: 'timeline', icon: '<path d="M12 3v18M7 7h5M12 12h5M7 17h5"/>' },
	{
		path: '/genome',
		key: 'nav_gen',
		mockup: 'genome',
		icon: '<path d="M7 3c0 6 10 6 10 12s-10 6-10 6M17 3c0 6-10 6-10 12"/><path d="M8 7h8M8 17h8"/>'
	},
	{
		path: '/drug-check',
		key: 'nav_drug',
		mockup: 'drug-check',
		icon: '<rect x="3" y="9" width="18" height="7" rx="3.5" transform="rotate(-35 12 12.5)"/><path d="m9 9 5 6"/>'
	},
	{
		path: '/wearables',
		key: 'nav_wear',
		mockup: 'wearables',
		icon: '<rect x="7" y="6" width="10" height="12" rx="3"/><path d="M9 6V3h6v3M9 18v3h6v-3"/>'
	},
	{ path: '/imaging', key: 'nav_img', mockup: 'imaging', icon: '<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="12" cy="12" r="4"/>' },
	{
		path: '/import-review',
		key: 'nav_imp',
		mockup: 'import-review',
		icon: '<path d="M12 3v12M7 10l5 5 5-5"/><path d="M4 17v3h16v-3"/>',
		countFromReview: true
	},
	// v0.2: the inbox registry and the quarantine (not a mockup screen; its tiles sit on #import-review).
	{
		path: '/inbox',
		key: 'fu_inbox',
		mockup: 'import-review',
		icon: '<path d="M4 13h4l2 3h4l2-3h4"/><path d="M5 5h14l1 8v6H4v-6z"/>'
	},
	{
		path: '/family',
		key: 'nav_fam',
		mockup: 'family',
		icon: '<circle cx="8" cy="8" r="3"/><circle cx="17" cy="9" r="2.5"/><path d="M3 20c0-3 2.5-5 5-5s5 2 5 5M13 20c0-2.5 1.8-4 4-4s4 1.5 4 4"/>'
	},
	{ path: '/mcp-access', key: 'nav_mcp', mockup: 'mcp-access', icon: '<rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>' }
];
