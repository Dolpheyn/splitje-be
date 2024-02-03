create table "groups"
(
    id       uuid primary key                                default uuid_generate_v1mc(),

    -- By applying our custom collation we can simply mark this column as `unique` and Postgres will enforce
    -- case-insensitive uniqueness for us, and lookups over `username` will be case-insensitive by default.
    --
    -- Note that this collation doesn't support the `LIKE`/`ILIKE` operators so if you want to do searches
    -- over `username` you will want a separate index with the default collation:
    --
    -- create index on "user" (username collate "ucs_basic");
    --
    -- select * from "user" where (username collate "ucs_basic") ilike ($1 || '%')
    --
    -- We're not doing that here since the Realworld spec doesn't implement a search function for users.
    name      text collate "case_insensitive" unique not null,

    created_at    timestamptz                            not null default now(),
    updated_at    timestamptz
);

-- And applying our `updated_at` trigger is as easy as this.
SELECT trigger_updated_at('"groups"');

