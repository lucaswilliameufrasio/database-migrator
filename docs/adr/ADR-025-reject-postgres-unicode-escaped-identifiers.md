# ADR-025: Rejeitar identificadores PostgreSQL com escapes Unicode em migrations

- **Status:** Aceita
- **Data:** 2026-11-11

## Contexto

O guard de migrations PostgreSQL compara o texto de identificadores entre aspas
com os nomes protegidos das tabelas de metadados do Database Migrator. O
PostgreSQL resolve identificadores escritos como `U&"..."` antes da execução,
inclusive formas com cláusula `UESCAPE` customizada. O scanner não implementa
essa decodificação Unicode; assim, uma forma escapada como
`U&"\0073chema_migration_locks"` pode ocultar o nome protegido do guard,
enquanto o PostgreSQL o resolve como `schema_migration_locks`.

## Decisão

- Rejeitar, sem diferenciar maiúsculas/minúsculas, identificadores PostgreSQL
  entre aspas com escape Unicode antes das verificações de segurança. A forma
  `UESCAPE` também é coberta, pois vem depois do prefixo rejeitado `U&"..."`.
- Continuar aceitando identificadores entre aspas com caracteres Unicode
  literais; por exemplo, `"café"` continua válido.
- Não rejeitar strings com escapes Unicode, como `U&'...'`: elas não são
  identificadores e não ocultam nomes de tabelas de metadados do guard.
- Retornar um erro de segurança que explique que identificadores escapados não
  são aceitos porque podem ocultar nomes protegidos; sugerir Unicode literal.

## Alternativas consideradas

- **Decodificar todos os identificadores PostgreSQL escapados:** preserva o
  recurso completo do PostgreSQL, mas exige suporte correto a escapes padrão,
  `UESCAPE` customizado, escapes malformados e normalização de identificadores
  em um scanner de segurança que não é um parser PostgreSQL completo. Rejeitada
  para esta fronteira.
- **Rejeitar todo identificador não ASCII ou entre aspas:** é mais amplo que o
  necessário e quebraria nomes legítimos como `"café"`. Rejeitada.

## Consequências e limites

- Autores de migrations e geradores devem emitir caracteres UTF-8 literais em
  vez de identificadores entre aspas com escapes Unicode. Esta é uma restrição
  intencional e documentada da linguagem de migrations PostgreSQL.
- Identificadores com Unicode literal entre aspas e strings com escapes Unicode
  não são afetados. O guard continua rejeitando formas literais de nomes de
  tabelas de metadados protegidas.
- Reavaliar esta decisão somente se um lexer PostgreSQL completo e testado puder
  decodificar todas as formas suportadas de `U&`/`UESCAPE` antes da verificação
  de nomes reservados.
