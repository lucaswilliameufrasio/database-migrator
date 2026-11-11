# ADR-024: Fixar objetos validados em `init`, `new` e no runner

- **Status:** Aceita
- **Data:** 2026-11-10

## Contexto

`init` e `new` validavam pathnames e depois repetiam operações por pathname. Uma
troca de symlink em diretório ancestral podia alterar o destino entre a
verificação e a escrita. De forma semelhante, o runner era hasheado e iniciado
por pathname em operações separadas. O runner continua sendo código nativo
confiável do projeto com autoridade do processo; o protocolo não o transforma
em sandbox.

## Decisão

- Abrir o diretório raiz do projeto como capability e fazer as operações de
  `init` e as reservas/novos arquivos de `new` relativas a handles de diretório.
  A limpeza das reservas também usa o mesmo handle. A criação atômica e
  exclusiva de arquivos continua impedindo sobrescrita de folhas existentes.
- Antes da execução do runner, copiar seus bytes para um diretório temporário
  privado, calcular o fingerprint dessa cópia e executar esse mesmo arquivo.
  Em Unix, o diretório e o executável recebem permissões `0700`; o snapshot é
  removido ao sair do escopo que mantém o processo.
- A política de transporte plaintext não trata um nome DNS, inclusive
  `localhost`, como prova de loopback. PostgreSQL exige IP numérico de loopback
  ou `hostaddr` explícito correspondente; MySQL/MariaDB exigem IP numérico de
  loopback ou lista `resolved_ips` não vazia em que todos os IPs sejam loopback.

## Alternativas consideradas

- **Recanonicalizar imediatamente antes da escrita:** rejeitada como garantia
  suficiente porque ainda deixa uma janela entre revalidação e uso.
- **Usar apenas o pathname canônico do runner:** rejeitada porque um caminho
  canônico ainda pode ser substituído após o hash.
- **Resolver `localhost` e depois conectar novamente pelo nome:** rejeitada
  porque o driver poderia resolver para um conjunto diferente. O plaintext
  local usa endereço numérico ou endereço explicitamente fixado.

## Consequências e limites

- Desenvolvedores que usavam plaintext com `localhost` devem mudar para
  `127.0.0.1`/`::1`, ou fixar endereços loopback conforme o driver. TLS continua
  sendo o comportamento padrão para nomes de host.
- O snapshot liga o hash aos bytes iniciados, mas não restringe as capacidades
  do runner depois do início; isolamento de código nativo não é prometido.
- Handles de diretório protegem as mutações cobertas contra redirecionamento por
  pathname. Não demonstram uma fronteira contra processos com a mesma identidade
  capazes de renomear objetos abertos, nem substituem revisão de permissões,
  ACLs ou comportamento de filesystem do deployment.
- Testes de corrida usam projetos e destinos temporários descartáveis; nenhum
  banco compartilhado ou serviço real é acessado.
