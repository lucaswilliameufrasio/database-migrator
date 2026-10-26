# ADR-013: Journal, recuperação e evolução de metadata no SQLite

- **Status:** Aceita para F3; limitada a migrations SQLite transacionais
- **Data:** 2026-10-24

## Contexto

O primeiro incremento grava somente operações concluídas e usa metadata formato
1. F3 precisa registrar lifecycle de tentativas, steps SQL, locks diagnósticos,
diagnóstico read-only e reconciliação. SQLite só aceita migrations dentro da
transação gerenciada; um crash pode deixar o processo sem resposta, mas não deve
separar os efeitos do SQL do estado aplicado.

## Decisão

- Evoluir o schema para metadata formato 2. `up`, `down`, `status`, `plan` e
  `doctor` nunca fazem upgrade implícito. O formato 1 só pode ser convertido por
  `dbm metadata upgrade --yes`, que preserva as linhas de histórico e executa
  atomicamente. Formatos desconhecidos ou metadata parcial falham fechados.
- Registrar cada tentativa antes de executar SQL. A transição para `succeeded`,
  os steps reconhecidos e a alteração de `schema_migrations` são confirmados na
  mesma transação SQLite. Após rollback confirmado, registrar `failed` fora da
  transação. Um erro de commit/rollback mantém o lock diagnóstico ativo e exige
  `doctor`/reconciliação; não há retry automático.
- Dividir scripts em instruções sem separar delimitadores dentro de strings,
  comentários ou corpos `CREATE TRIGGER`. Persistir hash, índice, categoria e
  estado do step; nunca guardar SQL ou binds.
- Usar um lock diagnóstico por arquivo de banco, além de `BEGIN IMMEDIATE` como
  lock nativo. O registro ativo bloqueia novos writes; TTL não libera locks.
  `doctor` é read-only e não remove locks nem altera metadata.
- `doctor reconcile` requer resultado declarado, arquivo de evidência, plano de
  reparo JSON versionado, motivo, ator e `--approve`. O plano enumera, em ordem,
  cada índice/hash de statement, seu estado confirmado e uma nota de inspeção;
  hash divergente, estado incompatível ou step indeterminado bloqueia a ação.
  No SQLite, só aceita reconciliação compatível com o estado corrente
  demonstrável; persiste o plano, registra a intervenção e libera o lock
  correspondente. A referência SHA-256 é persistida para a evidência, não seu
  conteúdo.
- Esta fase não habilita `transaction=never`, retomar DDL parcial, produção,
  PostgreSQL ou MySQL/MariaDB. Esses contratos exigem drivers e gates próprios.

## Alternativas consideradas

- **Upgrade automático ao abrir o banco:** rejeitado porque `status`, `plan` e
  operações normais não podem alterar metadata silenciosamente.
- **Manter metadata v1 e gravar journal em arquivos laterais:** rejeitado por
  criar duas fontes de verdade e não atomicidade com o banco.
- **Tratar lock SQLite persistente como substituto do lock nativo:** rejeitado;
  a linha é diagnóstica/fail-closed, enquanto `BEGIN IMMEDIATE` serializa SQL.
- **Executar scripts com `execute_batch` sem steps:** rejeitado porque não
  fornece evidência por instrução para `doctor`.

## Consequências

- Bancos formato 1 permanecem intactos até upgrade explícito; a conversão é
  transacional e preserva as runs anteriores como `succeeded`.
- Uma interrupção abrupta pode exigir aprovação humana mesmo quando o estado
  transacional permite provar o resultado. Isso é preferível a reaplicar SQL às
  cegas.
- O journal de SQLite prova apenas execução transacional local. DDL não
  transacional e bancos remotos continuam bloqueados até homologação própria.

## Revisitar quando

Rever esta decisão ao adicionar o primeiro driver com DDL implicit-commit ou
quando o formato de metadata v2 precisar evoluir; nesses casos, preservar
compatibilidade com o journal e oferecer upgrade explícito.
