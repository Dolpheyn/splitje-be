create type txT as enum ('CREDIT', 'DEBIT');
create type ackT as enum ('NOT_ACK', 'ACK');

create table "transactions" (
    id            uuid   primary key default uuid_generate_v1mc(),

    amount        bigint not null,
    metadata      json   not null,
    tx_type       txT    not null,
    ack_status    ackT   not null,

    payer_id      uuid   not null references users(id),
    payee_id      uuid   not null references users(id),
    group_id      uuid   not null references groups(id),



    created_at    timestamptz                            not null default now(),
    updated_at    timestamptz
);

SELECT trigger_updated_at('"transactions"');
