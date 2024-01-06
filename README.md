# Splitje

## Setting Up

### Local Database

> Note: might be some steps missing e.g. installations. Help fill me up with docs!

spawn local db image with docker:
```sh
docker-compose up -d
```

run migrations
```sh
cargo sqlx migrate run
```

run prepare

