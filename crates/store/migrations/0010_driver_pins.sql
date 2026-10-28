-- External driver registrations pin the protocol and cookie versions and the
-- capabilities they were registered with. Registrations from before spoke the
-- only protocol there was, `openporch-http/1`, with cookie version 1 and no
-- optional capabilities, which is now written down.
UPDATE drivers
SET body = json_set(
    body,
    '$.protocol_version', 'openporch-http/1',
    '$.cookie_version', 1,
    '$.capabilities', json('{"observe":false,"recover":false,"preview":false,"cancel":false}')
)
WHERE json_valid(body) AND json_extract(body, '$.protocol_version') IS NULL;
