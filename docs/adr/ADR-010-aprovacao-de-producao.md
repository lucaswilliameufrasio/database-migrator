# ADR-010: Aprovação de mutações em produção

- **Status:** Aceita
- **Data:** 2026-11-12
- **Confirmação:** o plano mestre, seção 11.5, já fixava o contrato; o pedido de deixar a crate utilizável em produção autorizou implementá-lo.

## Contexto

`environment = production` é uma escolha explícita do operador. Até este incremento, toda mutação nesse ambiente falhava fechada com um bloqueio genérico. O plano exige que uma operação mutável em produção apresente o `plan_hash` da operação, a impressão digital do destino e uma aprovação explícita. Não existe `--force`.

## Decisão

- `development` e `test` continuam aplicando `up` e `down` sem aprovação extra. Flags de aprovação nesses ambientes são recusadas.
- Em `production`, `up` exige `--approve-plan` igual ao `plan_hash` de `dbm plan` e `--approve-target` igual a `target_fingerprint` do mesmo plano.
- `down` exige o `plan_hash` de `dbm plan --down --steps N` para o mesmo N. Esse hash inclui o plano base e a quantidade de passos, então a aprovação de `up` não autoriza rollback.
- A comparação ocorre depois da leitura e antes da primeira abertura de escrita. Plano, status, histórico e doctor continuam sem efeito durável.
- `metadata upgrade`, `doctor reconcile`, `import apply` e `import adopt` mantêm as aprovações próprias de cada operação e, em produção, também exigem `--approve-target` do plano read-only corrente.
- O runner compilado só recebe a execução depois que a CLI confere os mesmos valores. O processo do runner volta a conferir o plano vivo antes de escrever.

## Alternativas consideradas

- **Manter o bloqueio total:** rejeitada. Impedia o uso operacional que o plano já descrevia e que o operador pediu.
- **Um `--yes` genérico:** rejeitada. Não amarra a aprovação ao plano nem ao destino, e o plano proíbe override genérico.
- **Assinatura criptográfica do plano:** adiada. O contrato atual é a igualdade dos hashes publicados pelo plano read-only. Uma assinatura externa pode ser acrescentada sem reabrir o bloqueio total.

## Consequências

- Um `up` ou `down` de produção sem os dois hashes, ou com hashes de outro plano ou outro destino, não abre o banco para escrita.
- O operador precisa rodar o plano de novo quando o arquivo, o estado aplicado ou a identidade do servidor mudam.
- Texto puro em produção continua proibido. Esta decisão não fecha G11 nem G12 e não autoriza tag ou publicação.

## Como reverter

Voltar a recusar toda mutação com `environment = production` em `require_mutation_environment`, sem aceitar hashes de aprovação.
