#Git Setup
git config --global user.name "Md Nayem"
git config --global user.email 113716708+jsnayem@users.noreply.github.com

#Set default branch for Git to main
git config --global init.defaultBranch main

#Check Git Info
git config --get user.name
git config --get user.email

#check ssh key is available
ls ~/.ssh/id_ed25519.pub

#Generate ssh key
ssh-keygen -t ed25519

#Show ssh pubkey on terminal
cat ~/.ssh/id_ed25519.pub

#Check Github ssh connection
ssh -T git@github.com

#Clone a Repo
git clone git@github.com:jsnayem/workflows.git

#Git Origin Check
git remote -v

git status

git add Git.md

git commit -m "Add Git.md"

git log

git add .

git push

git push origin main