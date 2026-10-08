package api

func HealthHandler(w http.ResponseWriter, r *http.Request) { w.WriteHeader(200) }

type UserHandler struct{ DB *sql.DB }
