-- The profile an inbox filed each item to, so a profile delete finds the raw bytes of an item that made
-- no document (a mail with only a body keeps its .eml). Null on rows older than this file and on items
-- filed to nobody (held mail); a profile delete still finds those through their document or bytes.
alter table source_item add column profile_id uuid references profile on delete set null;
create index source_item_profile_idx on source_item (profile_id) where profile_id is not null;
