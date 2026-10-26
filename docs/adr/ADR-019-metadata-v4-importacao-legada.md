# ADR-019: Metadata v4 relacional para importação legada

- **Status:** Aceita para F8
- **Data:** 2026-10-24

## Contexto

O preview F8 normaliza Knex, Goose e SQLx, mas o metadata formato 3 só aceita
migrations nativas e não persiste a timeline completa nem a proveniência
necessária para aplicar e verificar um cutover. Importar não pode executar SQL
legado, converter um digest atual em prova histórica ou liberar rollback sem
adoção verificada.

## Decisão

- Evoluir explicitamente o contrato lógico para metadata formato 4 nos quatro
  adapters suportados. Instalações em formatos anteriores continuam legíveis
  quando já permitido, mas qualquer escrita/importação exige `metadata upgrade`
  explícito; leitura e planejamento nunca fazem upgrade automático.
- `schema_migrations` passa a representar estado atual nativo e legado. A linha
  legada mantém `kind=legacy`, a versão/identidade originais, o digest não
  reversível da identidade da origem, proveniência, checksums com algoritmo
  original, ordem observada e lacunas como `NULL`; a chave é derivada de um
  descriptor legado canônico. Linhas nativas existentes preservam suas chaves e
  valores durante o upgrade.
- Persistir a operação de importação e a timeline original em tabelas relacionais
  próprias: um registro de importação contém tool, identidade anonimizada da
  origem, digest do snapshot/plano, instante de observação e contagens; linhas de
  eventos guardam cada evento ordenado (incluindo reversões, falhas e sentinelas).
  Uma tabela de adoções registra vínculo, evidência, ator e aprovação sem apagar
  a trilha importada.
- `import apply` exige o digest aprovado, adquire o lock nativo da instalação,
  relê a origem sob lock, recalcula o plano/digest e só então grava metadata,
  estado corrente e proveniência. A operação não executa SQL, JS, Go ou código
  Rust legado e não escreve nas tabelas do migrator antigo.
- `import verify` compara a origem observada, a timeline persistida, as linhas
  atuais importadas e seus digests. Divergência bloqueia e não corrige os dados.
- Todo estado `legacy` bloqueia rollback. A adoção exige migration nativa
  correspondente, teste/evidência externa, revisão e aprovação; somente a
  transição auditada para estado nativo pode permitir o Down registrado pelo
  engine. Um hash do arquivo convertido não é tratado como prova do conteúdo
  executado historicamente.
- Upgrades são explícitos e preservam todas as linhas e eventos v3. Metadata
  parcial/desconhecida, identidade duplicada, fonte alterada desde a aprovação,
  lock legado ativo/desconhecido ou histórico falho não reconciliado falham
  fechados.

## Alternativas consideradas

- **Armazenar somente snapshot JSON:** rejeitada porque dificulta consulta,
  verificação de constraints e validação relacional de eventos e estado atual.
- **Manter estado legado em tabela separada da tabela de migrations:** rejeitada
  porque criaria duas fontes de verdade para o estado corrente e exigiria que
  planner, status e rollback mantivessem semânticas paralelas.
- **Importar/reexecutar SQL legado para reconstruir estado:** rejeitada porque
  histórico não é código autorizado para execução e não prova o estado real do
  banco.
- **Atualizar metadata antigo automaticamente:** rejeitada porque leitura e
  planejamento devem permanecer read-only e upgrades alteram dados persistidos.

## Consequências e evidência necessária

- DDL e codecs de metadata v4 precisam permanecer equivalentes entre SQLite,
  PostgreSQL, MySQL e MariaDB; MySQL/MariaDB continuam sujeitos a commit
  implícito e exigem lock de sessão pinned.
- O upgrade v3→v4 (e upgrades diretos já suportados de v1/v2) deve provar
  preservação de identidade, ordem, runs e steps e falha atômica onde o banco
  suporta transações DDL. Um upgrade parcialmente confirmado falha fechado.
- G8 requer fixtures reais por adapter/ferramenta, repetição/concorrrência de
  `apply`, prova de ausência de SQL legado, verificação de drift e teste negativo
  de rollback não adotado. SQLite local sozinho não homologa os outros adapters.
- A nova forma física ainda é provisória até G12; qualquer evolução posterior
  deve ser outro upgrade versionado, nunca edição retroativa do significado de
  registros existentes.
