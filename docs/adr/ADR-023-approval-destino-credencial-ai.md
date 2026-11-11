# ADR-023: Aprovação externa do destino da credencial AI

- **Status:** Aceita
- **Data:** 2026-11-10
- **Confirmação:** o usuário escolheu exigir aprovação pelo ambiente externo do processo.

## Contexto

Os profiles AI são lidos do projeto e contêm endpoint, referência da variável de
ambiente da chave e limites de execução. Em uma chamada explícita, a CLI pode
estar executando um projeto cuja configuração não é confiável enquanto o
processo possui uma credencial BYOK. Se o profile for a única autoridade do
destino, uma alteração de configuração pode direcionar essa credencial a outro
endpoint permitido.

## Decisão

- Toda chamada real do provider exige `DBM_AI_APPROVED_ENDPOINT`, fornecida pelo
  ambiente confiável que invoca o processo. O valor precisa corresponder ao
  endpoint base do profile selecionado; diferenças apenas em barras finais são
  normalizadas.
- A comparação acontece antes da leitura da variável da chave API. O URL da
  requisição é construído a partir do endpoint aprovado pelo ambiente.
- Preview de contexto não exige aprovação nem lê o segredo porque não cria o
  provider e não faz uma requisição.
- Perfis e comandos continuam opt-in; não há chamadas implícitas durante outros
  comandos.

## Alternativas consideradas

- **Confiar somente no endpoint do profile:** rejeitada porque o arquivo do
  projeto não é uma autoridade apropriada para selecionar o destino de uma
  credencial fornecida pelo processo.
- **Aceitar qualquer endpoint HTTPS:** rejeitada; transporte criptografado não
  prova que o proprietário da credencial aprovou o destinatário.
- **Bloquear todos os endpoints remotos:** rejeitada porque removeria o uso BYOK
  remoto já previsto pelo ADR-021.

## Consequências e evidência

- Invocadores AI passam a fornecer a variável de aprovação junto da variável da
  chave. Projetos que executam AI automaticamente devem obter a aprovação de uma
  fonte controlada pelo operador, não do próprio profile.
- Endpoint ausente, inválido ou divergente falha antes de ler a chave e antes de
  abrir conexão. Erros não incluem a chave nem valores do ambiente.
- O endpoint aprovado não é um segredo. A credencial continua sendo lida no
  momento da chamada, enviada somente no cabeçalho Authorization e excluída de
  logs e relatórios.
- Testes usam uma chave sintética e um provider local falso; nenhum provider
  real é chamado.
- Rever esta decisão se o BYOK passar a usar referências de segredo que carreguem
  uma política de destinatário autenticada e independente da configuração do
  projeto.
