# Tasks

## run

Run server and serve files from directory.

```sh
$MASK build
docker run --rm=true -p 8080:80 lunemec/eve-looter:latest
```

## build

Build docker image.

```sh
docker build --platform="linux/amd64" . -t lunemec/eve-looter:latest
```

## push

Pushes image to registry.

```sh
docker push lunemec/eve-looter:latest
```
