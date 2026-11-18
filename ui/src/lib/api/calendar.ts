// The calendar feed of one profile's due items (backend: routes/ics.rs). The feed URL itself is
// only in the create answer; the list shows the prefix.
import { del, get, p, post, request } from './core';
import type { Uuid } from './types';

export type FeedDetail = 'generic' | 'named';
export interface CalendarFeed {
	id: Uuid;
	prefix: string;
	detail: FeedDetail;
	created_at: string;
	last_used_at: string | null;
}

export const listCalendarFeeds = (slug: string) => get<{ items: CalendarFeed[] }>(`${p(slug)}/calendar-feeds`);
/** `path` is `/api/ics/<token>.ics`: shown once, never listed again. */
export const createCalendarFeed = (slug: string, detail: FeedDetail) =>
	post<{ feed: CalendarFeed; path: string }>(`${p(slug)}/calendar-feeds`, { detail });
export const setCalendarFeedDetail = (slug: string, id: Uuid, detail: FeedDetail) =>
	request<CalendarFeed>('PATCH', `${p(slug)}/calendar-feeds/${id}`, { body: { detail } });
export const revokeCalendarFeed = (slug: string, id: Uuid) => del<{ id: Uuid; revoked: true }>(`${p(slug)}/calendar-feeds/${id}`);
