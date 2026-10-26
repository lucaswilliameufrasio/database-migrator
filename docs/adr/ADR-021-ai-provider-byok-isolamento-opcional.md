# ADR-021: Provider BYOK opcional e geração de propostas de migration

- **Status:** Aceita
- **Data:** 2026-10-25
- **Confirmação:** dependência HTTP opcional aprovada pelo usuário durante a implementação de F10.

## Contexto

F10 exige provedores substituíveis, BYOK, contexto seguro, schemas de resposta,
preview do contexto e ausência de qualquer caminho de aplicação automática. A
CLI e o crate padrão devem continuar úteis sem credenciais ou chamadas externas.
O plano existente especifica compatibilidade OpenAI, profiles, secrets por
referência e saída de propostas, mas não escolhe o cliente HTTP nem a fronteira
de habilitação.

## Decisão

- Implementar o provider OpenAI-compatible com cliente `reqwest` atrás da
  feature Cargo opcional `ai`; builds sem essa feature não incluem o transporte
  nem expõem comandos que possam fazer chamadas de IA.
- `ai.enabled` é `false` por padrão. Cada chamada também exige um comando
  explícito `dbm ai ...`; não há chamadas durante `init`, `validate`, `plan`,
  `up`, `down`, `status`, `doctor`, importação ou carregamento da biblioteca.
- Profiles guardam endpoint, modelo, referência ao nome da variável de ambiente
  do segredo e limites de timeout/tokens. O valor do segredo só é lido no
  momento da requisição, enviado exclusivamente no cabeçalho de autorização e
  nunca serializado, impresso ou incluído em erros.
- `MigrationProposal` e relatórios de review/explain são dados não confiáveis,
  validados contra schemas estritos. As operações AI só produzem JSON/propostas;
  não escrevem migrations locais, não alteram metadata e não oferecem `--apply`.
- `--preview-context` constrói e apresenta exatamente o contexto que seria
  enviado, sem criar cliente HTTP ou abrir conexão. O contexto inicial usa
  somente dialect, migration sources locais selecionadas e pedido do usuário;
  não inclui linhas/dados de aplicação, connection URLs, segredos ou introspecção
  remota implícita.
- Sem retry automático de requests de geração, para não repetir custos ou
  chamadas após resultado ambíguo. Timeouts e rate limits são erros tipados e
  redigidos.

## Alternativas consideradas

- **Cliente HTTP obrigatório no binário padrão:** rejeitado porque tornaria a
  integração AI e sua superfície de dependências obrigatórias para todos os
  usuários.
- **Somente trait/provider injetado pelo aplicativo:** rejeitado como entrega
  F10 porque não oferece BYOK via CLI nem o endpoint OpenAI-compatible previsto.
- **Shell-out para `curl`:** rejeitado por introduzir uma dependência de runtime,
  dificultar timeout/redaction e ampliar exposição por argumentos/processos.
- **Aplicar arquivos ou SQL retornados pelo modelo:** rejeitado por violar a
  fronteira de aprovação humana e misturar proposta com mutação.

## Consequências e evidência necessária

- A feature `ai` precisa compilar separadamente e junto com `--all-features`; o
  build padrão não pode abrir conexões nem resolver segredo AI.
- Testar o cliente de produção contra servidor HTTP local falso: autenticação,
  schema de request/response, JSON malformado, status 429/5xx, timeout, corpo
  grande e redaction. Não usar provider real no gate normal.
- Confirmar que preview não usa rede, que saídas não contêm o segredo e que
  nenhuma resposta pode acionar `up`, `down`, escrita em source ou alteração de
  metadata.
- Rever o escopo de introspecção quando houver um contrato read-only de catálogo
  por driver; qualquer contexto de banco deverá continuar mínimo e opt-in.
- Reconsiderar a dependência se um futuro provider nativo ou um transporte comum
  aos adapters reduzir substancialmente o custo sem perder isolamento.
