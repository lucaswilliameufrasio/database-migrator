# ADR-001: Identidade nativa e checksum de migrations

- **Status:** Aceita para o primeiro incremento (SQL + SQLite)
- **Data:** 2026-10-23

## Contexto

Uma migration aplicada precisa continuar identificável mesmo se o arquivo for
renomeado ou editado. O plano mestre também exige timestamps UTC de 20 dígitos
e checksum do conteúdo sem normalização.

## Decisão

- A versão nativa é `YYYYMMDDHHMMSSffffff` em UTC e é validada como data/hora
  real. O nome é `<version>_<description>.sql`; pares usam `.up.sql` e
  `.down.sql`.
- A identidade técnica inicial é um SHA-256 de um descriptor com formato
  versionado, namespace e versão. Versão, nome e caminho continuam armazenados
  separadamente para diagnóstico.
- Single-file usa SHA-256 dos bytes exatos do arquivo (`sha256:raw:v1`).
- Pares usam SHA-256 com campos identificados e prefixos de tamanho sobre os
  bytes exatos de Up e Down (`sha256:pair:v1`).
- Edição, renomeação, remoção ou mudança de formato de uma migration aplicada
  é divergência; nunca atualiza checksum silenciosamente.
- `new` serializa criadores no diretório de origem e também usa uma reserva
  exclusiva por timestamp. Colisão ou reserva abandonada falha fechada; nenhum
  arquivo é sobrescrito.

## Consequências

O hash exato detecta até mudanças em comentários e line endings. O formato de
hash é interno e ainda pode evoluir antes de uma versão pública estável; uma
mudança futura exigirá versionamento explícito.

## Evidências

Testes cobrem validação do timestamp, checksum raw, colisão, renomeação e
divergência de conteúdo.
