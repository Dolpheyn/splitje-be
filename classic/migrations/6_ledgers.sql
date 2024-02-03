create table "ledgers" (
    id            uuid primary key                   default uuid_generate_v1mc(),

    amount        bigint not null default 0,

    this_user     uuid not null references users(id),
    other_user    uuid not null references users(id),
    group_id      uuid not null references groups(id),



    created_at    timestamptz not null default now(),
    updated_at    timestamptz
);

SELECT trigger_updated_at('"ledgers"');
