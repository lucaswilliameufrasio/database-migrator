# ADR-016: Execução MySQL/MariaDB por step e sessão pinned

- **Status:** Aceita; G5 homologado localmente em 2026-10-24; API interna provisória
- **Data:** 2026-10-24

## Contexto

MySQL e MariaDB expõem `mysql_async`, mas DDL causa commit implícito e não pode
ser anunciado como revertível por uma transação que engloba a migration. A FSM
atual já registra intent/ack por statement quando uma migration é explicitamente
`never`; o lock nativo de nome é vinculado à sessão. Reutilizar a conexão de um
pool ou manter a execução transacional atual tornaria recovery e lock enganosos.

## Decisão

- Um target configurado como `driver = "mysql"` usa uma única `mysql_async::Conn`
  física, sem `Pool`, do lock até o fim da operação. O adaptador detecta MySQL ou
  MariaDB pelo servidor, mas ambos usam o dialect de fonte `mysql` nesta fatia.
- A serialização usa `GET_LOCK`/`RELEASE_LOCK` na sessão pinned. O nome é
  determinístico, ASCII e limitado a 64 caracteres; connection ID é verificado
  nas fronteiras. Perda da sessão não cria reconexão nem retry automático.
- Inicialmente, migrations MySQL/MariaDB precisam declarar explicitamente
  `-- dbm:transaction never`. `auto` e `always` falham antes de conectar/executar;
  o formato pareado é rejeitado porque não comporta essa diretiva. Não há
  downgrade silencioso de garantia. Cada statement recebe intent
  durável, execução isolada e ACK durável. DDL e demais efeitos confirmados não
  são agrupados em uma falsa transação de migration.
- Se a resposta do statement não prova se ele foi aplicado, o run fica
  indeterminado e bloqueia novas escritas. `doctor` não infere resultado pela
  ausência da linha aplicada. Reconciliação requer fonte/checksum, plano de
  reparo por statement, evidência externa, motivo, ator e aprovação explícita;
  não executa nem repete SQL.
- Metadata permanece na versão lógica 3, com DDL MySQL próprio em InnoDB,
  timestamps UTC com microssegundos e chaves de índice fixas em ASCII. Bootstrap
  usa lock nativo e, diante de um conjunto parcial de tabelas, falha fechado em
  vez de reparar implicitamente.
- TLS é obrigatório por padrão. Transporte plaintext requer opt-in apenas para
  host loopback em ambiente development/test; produção rejeita o opt-in.

## Alternativas consideradas

- **Executar a migration inteira dentro de `START TRANSACTION`:** rejeitada,
  pois DDL pode confirmar implicitamente e invalidar a atomicidade anunciada.
- **Converter `auto` em `never` sem aviso:** rejeitada porque esconderia uma
  mudança de garantia. A anotação explícita é parte da revisão de risco.
- **Usar `mysql_async::Pool` e confiar no cliente lógico:** rejeitada; `GET_LOCK`
  é de sessão e a conexão precisa permanecer pinned.
- **Inferir sucesso pelo estado global sem plano de reparo:** rejeitada; SQL
  arbitrário pode ter efeitos que não são inferíveis genericamente.

## Consequências e revisitar quando

- O primeiro escopo aceita SQL por statements com scanner MySQL/MariaDB e journal
  durável, mas não promete rollback atômico de uma migration nem autorreconcilia
  SQL arbitrário.
- A matriz de homologação é separada para MySQL 8.4 e MariaDB LTS; conexão
  bem-sucedida não é evidência suficiente de compatibilidade.
- Reavaliar `auto`/`always` quando houver classificador de statements e
  capabilities testadas para ambos os servidores. Qualquer suporte transacional
  novo precisa demonstrar que metadata e SQL permanecem na mesma fronteira
  atômica.

## Evidência de homologação G5

Em 2026-10-24, as jornadas CLI opt-in passaram em instâncias descartáveis
MySQL 8.4 e MariaDB 10.11.14, 11.4.13 e 11.8.2. Foram exercitados bootstrap e
metadata formato 3, plano somente leitura, Up/Down, lock concorrente e liberação
ao retornar da CLI, deadlock InnoDB preservando o lock nomeado da sessão, falha
no segundo e no terceiro de três statements, estado parcial, reconciliação
aprovada, retry somente após reconciliação e interrupção de processo durante SQL
com liberação do lock da sessão perdida. Cada versão MariaDB foi executada
separadamente.

TLS com CA local confiável e rejeição de hostname divergente passou em MySQL 8.4
e MariaDB 10.11.14. Essas evidências são testes locais opt-in, não uma promessa
de compatibilidade com versões fora da matriz nem uma autorização para uso em
produção. Os testes estão em `tests/cli.rs` e exigem variáveis de ambiente que
apontem exclusivamente para servidores descartáveis em loopback.
