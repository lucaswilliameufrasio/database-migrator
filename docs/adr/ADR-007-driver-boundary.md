# ADR-007: Fronteira interna do driver no primeiro incremento

- **Status:** Substituída pela ADR-014
- **Data:** 2026-10-23

## Contexto

O plano exige uma API pequena para adaptadores, mas a forma async/object-safe
ainda depende dos drivers PostgreSQL/MySQL. Implementar essa API pública antes
da PoC de drivers criaria um contrato sem evidência.

## Decisão

- A V0 organiza descoberta, planejamento, metadados e execução em módulos
  separados e mantém a fronteira do driver interna e síncrona para o CLI
  SQLite.
- Não declarar compatibilidade com drivers externos, API embedded estável ou
  async nesta etapa.
- Ao iniciar PostgreSQL, rever o contrato com conexão pinned e futures
  object-safe antes de torná-lo público; registrar nova decisão se necessário.

## Consequências

O crate inicial prioriza uma jornada funcional, não compatibilidade pública.
O caminho de refatoração deve ser demonstrado numa PoC de driver antes de
estabilizar a API. A revisão para F4 foi concluída na ADR-014; esta decisão
permanece como histórico da fronteira provisória da V0.
