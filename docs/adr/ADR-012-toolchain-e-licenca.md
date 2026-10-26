# ADR-012: Toolchain, MSRV e licença da V0

- **Status:** Aceita para início da implementação; reavaliar antes de publicar
- **Data:** 2026-10-23

## Contexto

O plano exige edition e MSRV explícitas, `Cargo.lock` reproduzível e propõe
licença dual MIT/Apache-2.0. A toolchain instalada no ambiente era 1.98.1;
Rust 1.99.1 foi a stable publicada mais recente verificada para esta decisão.

## Decisão

- Edition 2024; MSRV declarada 1.89 por ser a exigência do conjunto inicial de
  dependências (`time` e `sha2`).
- Fixar toolchain de desenvolvimento/CI em Rust 1.99.1; manter `Cargo.lock`
  versionado.
- Usar licença `MIT OR Apache-2.0`, conforme a proposta do plano, sujeita a
  revisão antes de publicação.
- Nome do binário `dbm` e nome do package permanecem provisórios; nenhum
  package será publicado nesta fatia.

## Evidências

Rust 1.99.1 foi verificado na página de releases do upstream em 2026-10-23;
`time` 0.3.55 e `sha2` 0.11.1 declaram MSRV 1.88, `uuid` 1.29.0 declara MSRV
1.89 e `clap` 4.6.7 declara MSRV 1.85.
